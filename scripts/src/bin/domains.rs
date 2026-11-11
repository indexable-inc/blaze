//! domains: every hash domain written in blaze's Rust is written once. The port of
//! `tools/v2/domains.py`, line for line: the same one JSON argument, lines printed and exit
//! status (docs/v2/plan/design/scripts.md, "Ports and their parity"). What follows is the
//! Python's docstring, which says what a domain is and the rule.
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
            domain: re(r#"\A"(blaze[ -][^"\\{}]*?[ -]\d+(?:\\0)?)""#),
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

fn sites(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Site>> {
    let mut found: BTreeMap<String, Vec<Site>> = BTreeMap::new();
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
        }
    }
    found
}

fn places(sites: &[&Site]) -> String {
    sites.iter().map(|it| format!("{}:{}", it.path, it.line)).collect::<Vec<_>>().join(", ")
}

fn problems(p: &Patterns, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let found = sites(p, root);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
