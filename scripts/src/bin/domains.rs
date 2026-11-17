//! domains: every hash domain written in blaze's Rust is written once. The port of
//! `tools/v2/domains.py`, deleted once both were run on the same tree and agreed
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"). What follows says what a domain
//! is and the rule.
//!
//!     blaze run -q -p scripts --bin domains -- '{"root": "."}'
//!
//! Checks that every hash domain written in blaze's Rust is written once: two hashes that share a
//! domain string are one hash to whoever compares their outputs (AGENTS.md one-concept-one-home).
//!
//! Input: `{"root": "<repository>"}` (inline, a path, or `-`). Prints one line per problem and exits
//! 1 when there is any; CI runs it beside the `docs` script's `check`.
//!
//! A domain is a `str` literal under `crates/` that starts with `blaze` and ends in its version
//! number (`"blaze v2 body code 1"`, `"blaze plane history 1"`), the form every domain in the tree
//! has. Byte strings (`b"..."`) are left out: they are file magics, and every hashing entry point
//! (`blaze_ids::Hasher::new`, `Domain::new`, `ContentId::of`) takes a `&str`. Comment lines are
//! left out too: a doc that names a domain does not write it.
//!
//! The rule: each domain is written at most once outside test code. Test code (a file under a
//! `tests` or `benches` directory, a `tests.rs` or `test.rs`, an item under `#[cfg(test)]` or
//! `#[cfg(all(test, ...))]`) may repeat a domain, its own or one it recomputes, as often as it
//! likes: its hashes are compared with nothing kept. A literal of the domain form that is not a
//! domain (a text file's header) is named in ALLOWED with the one place it is written and why; an
//! entry that no longer matches is itself a problem, so the list only shrinks.
//!
//! Since 2026-11-12 a literal that starts `blaze v2 ` is a domain whatever it ends in: over sixty
//! domains had no version number at their end (`"blaze v2 def map pages"`, `"blaze v2 impl
//! tables"`, `"blaze v2 serve no-op request"`), and the rule above did not see them.
//!
//! Two more rules, both against docs/v2/plan/design/caches.md (AGENTS.md `granular-caching`,
//! `keys-cover-every-input`, `guard-the-cause`):
//!
//! - **Every domain is in the inventory.** Each domain written outside tests, less its version
//!   number ([`stem`]), is listed once under "Domains" beside a cache of the table under "The
//!   caches", whose second cell is the grain of its key. A grain coarser than an item, a file
//!   or a unit (`crate`, `program`, `checkout`, `process`) must have a last cell that names
//!   its owner or its argument. So a new key is written down with its grain before it lands,
//!   and a new crate-level key has to be argued. A name listed that no domain has is a
//!   problem too.
//! - **Every hasher that is not BLAKE3 has a class.** A file under `crates/` that names
//!   SipHash, `DefaultHasher`, `RandomState`, `FxHasher`, `AHasher` or FNV's offset basis
//!   outside tests is under a path of the table under "Hash functions", whose third cell is
//!   the class of its use ([`CLASSES`]). A row no such file is under is a problem.
//! - **Every `BLAZE_` variable is classified.** A `"BLAZE_..."` literal outside tests names a
//!   variable blaze reads. One that selects between two caching paths is a row of the table
//!   under "Switches that select a caching path", whose last cell says who removes it and
//!   when; every other is named in that section's list. So a new switch between two forms of
//!   one cache is written down with its remover ([`env_problems`]).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
    pyfmt::repr_str,
    pystr::{is_word, lstrip, strip},
    regex::Regex,
    universal_newlines,
};

const SCRIPT: &str = "domains.py";
const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];

/// Literals of the domain form that are not hash domains, each with the one non-test file that
/// writes it and why it may equal a domain written elsewhere.
const ALLOWED: [(&str, &str, &str); 1] = [(
    "blaze v2 field order 1",
    "crates/blaze-items/src/field_order.rs",
    "the first line of a field order plan's text file (`HEADER`), never hashed as a domain; \
     the plan's ID has its own (`PLAN_DOMAIN`)",
)];

struct Args {
    root: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["root"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args { root: f.str("root", None)? })
    }
}

#[derive(Debug, Clone)]
struct Site {
    /// Relative to the root.
    path: String,
    line: usize,
    test: bool,
}

struct Patterns {
    /// Python's `DOMAIN` from its opening quote on; its lookbehind `(?<![\w"])` is checked in
    /// `domains_in`.
    domain: Regex,
    cfg_test: Regex,
    module: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        Patterns {
            // A `str` literal (not `b"..."`, not `r"..."`) of the domain form: no escapes or
            // format braces but a trailing `\0`, ending in a version number.
            // Or, of any ending, one that starts `blaze v2 ` (no `<`: a doc's placeholder).
            domain: re(r#"\A"(blaze[ -][^"\\{}]*?[ -]\d+(?:\\0)?|blaze v2 [^"\\{}<]+)""#),
            // Python's `re.match` with `$`: a line holds no newline, so `$` is its end.
            cfg_test: re(r"\A\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]\s*\z"),
            module: re(r"\A\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{"),
        }
    }

    /// `DOMAIN.finditer(line)`: each domain literal's text, left to right, matches never
    /// overlapping.
    fn domains_in<'l>(&self, line: &'l str) -> Vec<&'l str> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = line[from..].find('"').map(|at| from + at) {
            let free = !line[..at].chars().next_back().is_some_and(|c| is_word(c) || c == '"');
            match self.domain.captures(&line[at..]).filter(|_| free) {
                Some(found) => {
                    out.push(found.get(1).map_or("", |m| m.as_str()));
                    from = at + found.get(0).map_or(1, |m| m.end());
                }
                None => from = at + 1,
            }
        }
        out
    }
}

fn test_path(parts: &[String]) -> bool {
    let (name, dirs) = parts.split_last().map_or(("", &[][..]), |(name, dirs)| (name.as_str(), dirs));
    dirs.iter().any(|part| TEST_DIRS.contains(&part.as_str())) || TEST_FILES.contains(&name)
}

/// `(root / "crates").rglob("*.rs")`, sorted as Python sorts paths (part by part), each as its
/// parts from the root. Directories that are links are not entered, as rglob does not.
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
            if name.ends_with(".rs") {
                out.push(parts.clone());
            }
            if is_dir {
                todo.push(parts);
            }
        }
    }
    out.sort();
    out
}

/// What the tree holds: each domain's sites, and each file that names a hasher that is not
/// BLAKE3 outside tests, with the first line that does and what it names.
struct Found {
    domains: BTreeMap<String, Vec<Site>>,
    hashers: BTreeMap<String, (usize, &'static str)>,
    /// Each `BLAZE_` variable a literal names outside tests, and the first place that does.
    envs: BTreeMap<String, String>,
}

fn sites(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Site>> {
    scan(p, root).domains
}

fn scan(p: &Patterns, root: &Path) -> Found {
    let mut found: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    let mut hashers: BTreeMap<String, (usize, &'static str)> = BTreeMap::new();
    let mut envs: BTreeMap<String, String> = BTreeMap::new();
    for parts in rust_files(root) {
        if parts.iter().any(|part| part == "target") {
            continue;
        }
        let in_test_file = test_path(&parts);
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // `read_text(errors="replace")`: what is not UTF-8 becomes U+FFFD.
        let Ok(bytes) = std::fs::read(&full) else { fail(SCRIPT, &format!("{} could not be read", full.display())) };
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let lines: Vec<&str> = text.split('\n').collect();
        // From a `#[cfg(test)]` module on, the rest of the file is its tests (they come last);
        // a `#[cfg(test)]` on another item covers the item's first line.
        let mut tests_from: Option<usize> = None;
        let mut test_line: Option<usize> = None;
        for (at, line) in lines.iter().enumerate() {
            if p.cfg_test.is_match(line) {
                let following = lines[at + 1..].iter().find(|it| !strip(it).starts_with("#[")).copied().unwrap_or("");
                if p.module.is_match(following) {
                    tests_from = tests_from.or(Some(at));
                } else {
                    test_line = Some(at + 1);
                }
                continue;
            }
            if lstrip(line).starts_with("//") {
                continue;
            }
            let test = in_test_file || tests_from.is_some_and(|from| at >= from) || test_line == Some(at);
            for domain in p.domains_in(line) {
                found.entry(domain.to_owned()).or_default().push(Site { path: parts.join("/"), line: at + 1, test });
            }
            if !test && let Some(named) = hasher_in(line) {
                hashers.entry(parts.join("/")).or_insert((at + 1, named));
            }
            if !test {
                for name in envs_in(line) {
                    envs.entry(name.to_owned()).or_insert_with(|| format!("{}:{}", parts.join("/"), at + 1));
                }
            }
        }
    }
    Found { domains: found, hashers, envs }
}

/// The section of the inventory that classifies the `BLAZE_` variables.
const SWITCHES: &str = "Switches that select a caching path";

/// `text` when it is a variable's name, `BLAZE_` and then capitals, digits and `_`, at least one.
fn env_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("BLAZE_")?;
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')).then_some(text)
}

/// Each `"BLAZE_..."` literal of `line` that is a variable's name whole.
fn envs_in(line: &str) -> Vec<&str> {
    line.match_indices("\"BLAZE_")
        .filter_map(|(at, _)| {
            let rest = &line[at + 1..];
            env_name(&rest[..rest.find('"')?])
        })
        .collect()
}

/// The inventory's problems with the `BLAZE_` variables the tree names (`envs`, each with the
/// first place that names it): one that is in neither the table nor the list of [`SWITCHES`],
/// a switch whose row does not say who removes it, a name there that the tree no longer has.
fn env_problems(envs: &BTreeMap<String, String>, doc: &str) -> Vec<String> {
    let Some(lines) = section(doc, SWITCHES) else {
        return vec![format!("{LEDGER} has no `## {SWITCHES}` section")];
    };
    let mut out = Vec::new();
    let mut known: BTreeMap<&str, ()> = BTreeMap::new();
    for line in &lines {
        // A name is written alone or with the value that selects (`BLAZE_PURE=verify`).
        fn names(text: &str) -> Vec<&str> {
            ticked(text).into_iter().filter_map(|it| env_name(it.split('=').next().unwrap_or(it))).collect()
        }
        match cells(line) {
            Some(row) => {
                let switches = row.first().map(|it| names(it)).unwrap_or_default();
                if !switches.is_empty() && (row.len() < 3 || row.last().is_none_or(|it| it.is_empty() || *it == "-")) {
                    out.push(format!(
                        "{LEDGER}: the switch `{}` has no last cell: say who removes it and when, or why it stays",
                        switches[0]
                    ));
                }
                known.extend(switches.into_iter().map(|it| (it, ())));
            }
            None => known.extend(names(line).into_iter().map(|it| (it, ()))),
        }
    }
    for (name, at) in envs {
        if !known.contains_key(name.as_str()) {
            out.push(format!(
                "the variable `{name}` ({at}) is not in {LEDGER}: if it selects between two caching paths (two forms of one cache, a cache on or off, an old key), add its row under \"{SWITCHES}\" with who removes it and when; if not, add it to that section's list of other variables"
            ));
        }
    }
    for name in known.keys() {
        if !envs.contains_key(*name) {
            out.push(format!("{LEDGER}: \"{SWITCHES}\" names `{name}`, which no file of crates/ names outside tests; remove it"));
        }
    }
    out
}

/// Where the inventory is, from the root.
const LEDGER: &str = "docs/v2/plan/design/caches.md";
/// The grains a cache's key may have, and those that have to be argued.
const GRAINS: [&str; 8] = ["item", "file", "unit", "crate", "program", "checkout", "process", "none"];
const COARSE: [&str; 4] = ["crate", "program", "checkout", "process"];
/// The classes a use of a hasher that is not BLAKE3 may have.
const CLASSES: [&str; 7] = ["identity", "probe", "required", "integrity", "random", "names", "unaudited"];
/// The hashers that are not BLAKE3, as a line names them; FNV by its offset basis, as it is
/// written by hand wherever it is used.
const HASHERS: [&str; 5] = ["SipHasher", "DefaultHasher", "RandomState", "FxHasher", "AHasher"];
const FNV_BASIS: [&str; 2] = ["cbf2_9ce4_8422_2325", "cbf29ce484222325"];

/// The hasher `line` names that is not BLAKE3, if it names one.
fn hasher_in(line: &str) -> Option<&'static str> {
    if let Some(named) = HASHERS.iter().find(|it| line.contains(**it)) {
        return Some(*named);
    }
    let lower = line.to_ascii_lowercase();
    FNV_BASIS.iter().any(|it| lower.contains(it)).then_some("FNV-1a")
}

/// A domain less its version number: what the inventory lists, so a version bump changes no
/// doc. `"blaze v2 item key 2"` and `"blaze v2 item key 3"` are both `blaze v2 item key`; a
/// domain with no number at its end is itself.
fn stem(domain: &str) -> &str {
    let body = domain.strip_suffix("\\0").unwrap_or(domain);
    let named = body.trim_end_matches(|c: char| c.is_ascii_digit());
    if named.len() == body.len() {
        return body;
    }
    match named.strip_suffix([' ', '-']) {
        Some(rest) if !rest.is_empty() => rest,
        _ => body,
    }
}

/// The text between each pair of backticks of `text`, in order.
fn ticked(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// A table row's cells, or none for a line that is no row or is the rule under the head.
fn cells(line: &str) -> Option<Vec<&str>> {
    let row = strip(line).strip_prefix('|')?.strip_suffix('|')?;
    let cells: Vec<&str> = row.split('|').map(strip).collect();
    (!cells.iter().all(|it| it.chars().all(|c| c == '-'))).then_some(cells)
}

/// The lines of `doc` under the heading `## <head>`, up to the next `## `.
fn section<'d>(doc: &'d str, head: &str) -> Option<Vec<&'d str>> {
    let mut lines = doc.split('\n').skip_while(|it| strip(it).strip_prefix("## ").map(strip) != Some(head));
    lines.next()?;
    Some(lines.take_while(|it| !it.starts_with("## ")).collect())
}

/// The inventory's problems with what the tree holds: `domains` with their sites, `hashers`
/// the files that name a hasher that is not BLAKE3, `doc` the inventory's text.
fn ledger_problems(
    domains: &BTreeMap<String, Vec<Site>>,
    hashers: &BTreeMap<String, (usize, &'static str)>,
    doc: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let (Some(table), Some(listed), Some(hashes)) =
        (section(doc, "The caches"), section(doc, "Domains"), section(doc, "Hash functions"))
    else {
        return vec![format!("{LEDGER} has no `## The caches`, `## Domains` or `## Hash functions` section")];
    };
    // The caches: a row's first cell is its name in backticks, its second its grain.
    let mut caches: BTreeMap<&str, ()> = BTreeMap::new();
    for row in table.iter().filter_map(|it| cells(it)) {
        let (Some(name), Some(grain), Some(last)) = (row.first().and_then(|it| ticked(it).first().copied()), row.get(1), row.last())
        else {
            continue;
        };
        if caches.insert(name, ()).is_some() {
            out.push(format!("{LEDGER}: the cache `{name}` has two rows"));
        }
        if !GRAINS.contains(grain) {
            out.push(format!(
                "{LEDGER}: the cache `{name}` has the grain {}, which is none of {}",
                repr_str(grain),
                GRAINS.join(", ")
            ));
        }
        if COARSE.contains(grain) && (row.len() < 3 || last.is_empty() || *last == "-") {
            out.push(format!(
                "{LEDGER}: the cache `{name}` is keyed a {grain} and its last cell is empty: name the lane that moves it to a key an item, or say why no finer key is possible (AGENTS.md granular-caching)"
            ));
        }
    }
    // The domains: `- `cache`: `name`, `name``, a name less `blaze v2 ` or whole after `=`.
    let mut placed: BTreeMap<String, &str> = BTreeMap::new();
    for line in listed.iter().filter(|it| it.starts_with("- `")) {
        let names = ticked(line);
        let Some((cache, names)) = names.split_first() else { continue };
        if !caches.contains_key(cache) {
            out.push(format!("{LEDGER}: \"Domains\" lists `{cache}`, which is no row of \"The caches\""));
        }
        for name in names {
            let whole = match name.strip_prefix('=') {
                Some(whole) => whole.to_owned(),
                None => format!("blaze v2 {name}"),
            };
            if let Some(other) = placed.insert(whole.clone(), *cache) {
                out.push(format!("{LEDGER}: the domain {} is listed under `{other}` and under `{cache}`", repr_str(&whole)));
            }
        }
    }
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for (domain, sites) in domains {
        let written: Vec<&Site> = sites.iter().filter(|it| !it.test).collect();
        // A literal with a backtick in it is a message, never a domain: the list cannot hold it.
        if written.is_empty() || domain.contains('`') {
            continue;
        }
        let name = stem(domain);
        seen.insert(name, ());
        if !placed.contains_key(name) {
            out.push(format!(
                "the hash domain {} ({}) is in no row of {LEDGER}: list {} under \"Domains\" beside the cache it keys, with that cache's row and the grain of its key in \"The caches\" (an entry is keyed an item, never a crate: AGENTS.md granular-caching)",
                repr_str(domain),
                places(&written),
                repr_str(name)
            ));
        }
    }
    for (name, cache) in &placed {
        if !seen.contains_key(name.as_str()) {
            out.push(format!("{LEDGER}: `{cache}` lists the domain {}, which nothing outside tests writes; remove it", repr_str(name)));
        }
    }
    // The hashers: a row's first cell is a path in backticks, its third the class.
    let mut paths: Vec<(&str, bool, bool)> = Vec::new();
    for row in hashes.iter().filter_map(|it| cells(it)) {
        let Some(path) = row.first().and_then(|it| ticked(it).first().copied()).filter(|it| it.starts_with("crates/")) else {
            continue;
        };
        let class = row.get(2).copied().unwrap_or("");
        if !CLASSES.contains(&class) {
            out.push(format!(
                "{LEDGER}: the hasher row `{path}` has the class {}, which is none of {}",
                repr_str(class),
                CLASSES.join(", ")
            ));
        }
        let blake3 = row.get(1).is_some_and(|it| it.starts_with("BLAKE3"));
        paths.push((path, blake3, false));
    }
    for (file, (line, named)) in hashers {
        let mut under = false;
        for (path, _, used) in &mut paths {
            if file.starts_with(*path) {
                (under, *used) = (true, true);
            }
        }
        if !under {
            out.push(format!(
                "{file}:{line} names {named}, a hasher that is not BLAKE3, and is under no path of \"Hash functions\" in {LEDGER}: a hash that stands for content is `blaze_ids::Hasher` under a domain; for a probe, a required format, a checksum or a seed, add the file's row with its class"
            ));
        }
    }
    for (path, blake3, used) in paths {
        if !used && !blake3 {
            out.push(format!("{LEDGER}: the hasher row `{path}` covers no file that names such a hasher outside tests; remove it"));
        }
    }
    out
}

fn places(sites: &[&Site]) -> String {
    sites.iter().map(|it| format!("{}:{}", it.path, it.line)).collect::<Vec<_>>().join(", ")
}

fn problems(p: &Patterns, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Found { domains: found, hashers, envs } = scan(p, root);
    match std::fs::read(root.join(LEDGER)) {
        Ok(bytes) => {
            let doc = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
            out.extend(ledger_problems(&found, &hashers, &doc));
            out.extend(env_problems(&envs, &doc));
        }
        Err(error) => out.push(format!("{LEDGER} could not be read ({error}): every hash domain is held to its row there")),
    }
    // A BTreeMap is in the order Python's `sorted(found.items())` gives.
    for (domain, where_) in &found {
        let mut written: Vec<&Site> = where_.iter().filter(|it| !it.test).collect();
        if let Some((_, path, _)) = ALLOWED.iter().find(|(it, _, _)| *it == domain.as_str()) {
            let others: Vec<&Site> = written.iter().copied().filter(|it| it.path != *path).collect();
            if others.len() == written.len() || others.is_empty() {
                let shown = if written.is_empty() { "none".to_owned() } else { places(&written) };
                out.push(format!(
                    "ALLOWED[{}] names {path}, which no longer shares the literal with a domain (non-test sites: {shown}): remove the entry",
                    repr_str(domain)
                ));
                continue;
            }
            written = others;
        }
        if written.len() > 1 {
            out.push(format!(
                "the hash domain {} is written {} times outside tests ({}): give one of them its own domain, or name the one constant both use",
                repr_str(domain),
                written.len(),
                places(&written)
            ));
        }
    }
    let mut allowed = ALLOWED.to_vec();
    allowed.sort_unstable();
    for (domain, path, _) in allowed {
        if !found.contains_key(domain) {
            out.push(format!("ALLOWED[{}] ({path}): no such literal is left; remove the entry", repr_str(domain)));
        }
    }
    out
}

fn main() {
    let first = std::env::args().nth(1).unwrap_or_default();
    let args = input::parse::<Args>(&first, SCRIPT);
    let p = Patterns::new();
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&args.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&args.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let found = problems(&p, &root);
    for problem in &found {
        println!("{SCRIPT}: {problem}");
    }
    if !found.is_empty() {
        std::process::exit(1);
    }
    let count = sites(&p, &root).len();
    println!("{SCRIPT}: {count} hash domains, each written once outside tests");
    println!("{SCRIPT}: each is under a cache of {LEDGER} with its grain, and each hasher that is not BLAKE3 has its class there");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree's domains for [`ledger_problems`]: each written once, outside tests.
    fn written(domains: &[&str]) -> BTreeMap<String, Vec<Site>> {
        domains
            .iter()
            .map(|it| ((*it).to_owned(), vec![Site { path: "crates/a/src/lib.rs".to_owned(), line: 1, test: false }]))
            .collect()
    }

    const DOC: &str = "# Caches\n\n## The caches\n\n| cache | grain | value | defect, owner |\n|---|---|---|---|\n\
        | `bodies` | item | a body's MIR | none |\n| `images` | crate | a crate's image | lane records |\n\n\
        ## Hash functions\n\n| where | function | class | why |\n|---|---|---|---|\n\
        | `crates/ids/` | BLAKE3 | identity | the rule |\n| `crates/a/src/table.rs` | FNV-1a | probe | a slot |\n\n\
        ## Domains\n\n- `bodies`: `body key`, `=blaze frozen body`\n- `images`: `image key`\n";

    fn hashers(files: &[&str]) -> BTreeMap<String, (usize, &'static str)> {
        files.iter().map(|it| ((*it).to_owned(), (3, "FNV-1a"))).collect()
    }

    /// The invariant: a tree whose every domain is listed under a cache with a grain, and
    /// whose every hasher that is not BLAKE3 is under a row with a class, has no problem; a
    /// version bump of a domain changes nothing.
    #[test]
    fn a_tree_the_inventory_covers_has_no_problem() {
        let table = hashers(&["crates/a/src/table.rs"]);
        for body in ["blaze v2 body key 1", "blaze v2 body key 22"] {
            let found = written(&[body, "blaze frozen body 3", "blaze v2 image key 7\\0"]);
            assert_eq!(ledger_problems(&found, &table, DOC), Vec::<String>::new());
        }
        assert_eq!(stem("blaze v2 goal 3 solver"), "blaze v2 goal 3 solver");
        assert_eq!(stem("blaze-b-2"), "blaze-b");
    }

    /// The controls, one planted at a time: a domain no row lists (a new key), a crate-keyed
    /// cache with no owner, a grain that is none, a name no domain has, a domain under two
    /// caches, a hasher in a file no row covers, a hasher row that covers nothing. Each is
    /// one problem that names the thing.
    #[test]
    fn each_planted_gap_is_one_problem_naming_it() {
        let table = hashers(&["crates/a/src/table.rs"]);
        let all = ["blaze v2 body key 1", "blaze frozen body 3", "blaze v2 image key 7"];
        let one = |found: &BTreeMap<String, Vec<Site>>, table: &BTreeMap<String, (usize, &'static str)>, doc: &str, names: &str| {
            let said = ledger_problems(found, table, doc);
            assert!(said.len() == 1 && said[0].contains(names), "{names}: {said:?}");
        };
        let mut new_key = all.to_vec();
        new_key.push("blaze v2 crate bodies key 1");
        one(&written(&new_key), &table, DOC, "\'blaze v2 crate bodies key 1\' (crates/a/src/lib.rs:1) is in no row");
        one(&written(&all), &table, &DOC.replace("| lane records |", "| |"), "`images` is keyed a crate and its last cell is empty");
        one(&written(&all), &table, &DOC.replace("| item |", "| body |"), "`bodies` has the grain \'body\'");
        one(&written(&all[..2]), &table, DOC, "`images` lists the domain \'blaze v2 image key\'");
        one(&written(&all), &table, &DOC.replace("- `images`: `image key`", "- `images`: `image key`, `body key`"), "is listed under `bodies` and under `images`");
        one(&written(&all), &hashers(&["crates/a/src/table.rs", "crates/b/src/key.rs"]), DOC, "crates/b/src/key.rs:3 names FNV-1a");
        one(&written(&all), &hashers(&[]), DOC, "`crates/a/src/table.rs` covers no file");
        // A domain only tests write is held to no row.
        let mut tested = written(&all);
        tested.insert("blaze v2 test only 1".to_owned(), vec![Site { path: "crates/a/src/tests.rs".to_owned(), line: 2, test: true }]);
        assert_eq!(ledger_problems(&tested, &table, DOC), Vec::<String>::new());
    }

    /// The invariant: every variable the tree names is a switch with its remover or in the
    /// list of the others. The controls, one at a time: a new variable, a switch with no
    /// remover, a name the tree no longer has.
    #[test]
    fn a_variable_is_a_switch_with_its_remover_or_listed() {
        let doc = "## Switches that select a caching path\n\n| variable | selects | who removes it |\n|---|---|---|\n\
            | `BLAZE_PAGES=1`, `BLAZE_ITEMS` (`off`) | pages | lane a, when measured |\n\nOther variables: `BLAZE_HOME`, `BLAZE_`.\n\n## Next\n";
        let tree = |names: &[&str]| -> BTreeMap<String, String> {
            names.iter().map(|it| ((*it).to_owned(), "crates/a/src/lib.rs:4".to_owned())).collect()
        };
        let all = ["BLAZE_PAGES", "BLAZE_ITEMS", "BLAZE_HOME"];
        assert_eq!(env_problems(&tree(&all), doc), Vec::<String>::new());
        let one = |envs: &BTreeMap<String, String>, doc: &str, names: &str| {
            let said = env_problems(envs, doc);
            assert!(said.len() == 1 && said[0].contains(names), "{names}: {said:?}");
        };
        one(&tree(&["BLAZE_PAGES", "BLAZE_ITEMS", "BLAZE_HOME", "BLAZE_OLD_KEY"]), doc, "the variable `BLAZE_OLD_KEY` (crates/a/src/lib.rs:4) is not in");
        one(&tree(&all), &doc.replace("| lane a, when measured |", "| |"), "the switch `BLAZE_PAGES` has no last cell");
        one(&tree(&all[..2]), doc, "names `BLAZE_HOME`, which no file");
        assert_eq!(envs_in(r#"var("BLAZE_HOME") == "BLAZE_home" || x == "BLAZE_" || y("BLAZE_A1_B")"#), ["BLAZE_HOME", "BLAZE_A1_B"]);
    }

    /// What a line names: the std and third-party hashers by their type, FNV by its basis
    /// however it is grouped; BLAKE3's own adapter is none of them.
    #[test]
    fn hashers_are_found_by_name_and_fnv_by_its_basis() {
        assert_eq!(hasher_in("let mut h = std::hash::SipHasher::new();"), Some("SipHasher"));
        assert_eq!(hasher_in("type S = rustc_stable_hash::StableSipHasher128;"), Some("SipHasher"));
        assert_eq!(hasher_in("let mut hash = 0xCBF2_9CE4_8422_2325u64;"), Some("FNV-1a"));
        assert_eq!(hasher_in("const B: u64 = 0xcbf29ce484222325;"), Some("FNV-1a"));
        assert_eq!(hasher_in("code.hash(&mut blaze_hash::Blake3Hasher(&mut hasher));"), None);
    }

    /// The domains with no version number at their end, which the rule did not see until
    /// 2026-11-12, are found; a doc's placeholder and a format string are not.
    #[test]
    fn a_v2_domain_with_no_version_is_found() {
        let p = Patterns::new();
        assert_eq!(p.domains_in(r#"Hasher::new("blaze v2 def map pages")"#), ["blaze v2 def map pages"]);
        assert_eq!(p.domains_in(r#""blaze v2 goal 3 solver""#), ["blaze v2 goal 3 solver"]);
        assert_eq!(p.domains_in(r#""blaze v2 <what> <n>" "blaze v2 {name}" "blaze other""#), Vec::<&str>::new());
    }

    /// The refinement: `domains_in` finds what Python's `DOMAIN.finditer` finds, the lookbehind
    /// included (each expectation is what Python 3.13 gives).
    #[test]
    fn domains_are_found_as_python_finds_them() {
        let p = Patterns::new();
        assert_eq!(p.domains_in(r#"Hasher::new("blaze v2 body code 1")"#), ["blaze v2 body code 1"]);
        assert_eq!(p.domains_in(r#"b"blaze magic 1" r"blaze raw 2" x"blaze x 3""#), Vec::<&str>::new());
        assert_eq!(p.domains_in(r#""blaze a 1\0", "blaze-b-2" "blaze {} 3""#), [r"blaze a 1\0", "blaze-b-2"]);
        assert_eq!(p.domains_in(r#""" "blaze q 1""#), ["blaze q 1"]);
        assert_eq!(p.domains_in(r#"("blaze plane history 1" , "blaze plane history 1")"#), ["blaze plane history 1", "blaze plane history 1"]);
    }
}
