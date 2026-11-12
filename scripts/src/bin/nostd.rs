//! nostd: the check of ledger row S4 and its progress meter. The port of `harness/v2/nostd.py`,
//! deleted once both were run on the same tree and agreed (docs/v2/plan/design/scripts.md,
//! "Ports and their parity"). `DOC` below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin nostd -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin nostd -- selftest
//!
//! Where this differs from the Python, on purpose: a file it cannot read (or a manifest or lock
//! file that is not TOML) ends the script with a line naming it, where the Python raised a
//! traceback; both exit 1.

/// What the usage error prints: harness/v2/nostd.py's docstring, with this script's commands.
const DOC: &str = r#"nostd.py: the check of ledger row S4 ("bstd is `#![no_std]` with its own sys and runtime; no
blaze crate links Rust's std"), and its progress meter (docs/v2/plan/design/sys.md, "Migration").

    blaze run -q -p scripts --bin nostd -- '{"command": "check"}'
    blaze run -q -p scripts --bin nostd -- '{"command": "progress"}'
    blaze run -q -p scripts --bin nostd -- '{"command": "list"}'
    blaze run -q -p scripts --bin nostd -- selftest

`harness/v2/nostd.json` is the ban list. Banned everywhere in `crates/` but the system layer
(`sys`: crates/blaze-sys) are `libc::` and Rust std's file, network, thread, process and clock
paths (`banned`). A module that still names one is listed in `not_yet_moved` with why; a listed
path is a file or a directory (a whole crate that has not started). The list only shrinks: it
is what is left of S4.

The input is one JSON object:

    command   check      S4 itself. Passes only when `not_yet_moved` is empty, no file outside
                         `sys` names a banned path, every crate of `crates/` is `#![no_std]`,
                         none names `extern crate std` outside its tests, and every package
                         the lock file gives a blaze crate is a blaze crate or one of
                         `no_std_packages`
              progress   the guard while the work is under way. Fails when a file that is not
                         listed names a banned path (a move went backwards, or new code
                         reached past blaze-sys), or a listed path names none (it moved: take
                         it off the list), or a crate in `no_std_crates` is no longer
                         `#![no_std]`. Prints what is left, by row
              list       prints every file that names a banned path, with its counts
    root      the checkout (".")
    bans      the ban list, under root ("harness/v2/nostd.json")

It reads source text, the lock file and the list; it builds nothing. A use is found by its
text (`libc::`, `std::fs`, a `std::{.. fs ..}` group), outside comments and outside tests (a
`tests.rs`, a `tests/`, `benches/` or `examples/` directory, and an inline `#[cfg(test)] mod`
to the end of its file):
tests are built on std's harness and are not what a blaze binary links. The lint that
refuses the same uses by name resolution is the `ambient` effect (std.md, S2), with blaze-sys
the one trusted crate; this script is the meter and the ledger's check.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
    os_error,
    pypath::rglob,
    pystr::lstrip,
    read_text, read_text_lossy,
    regex::Regex,
    toml,
};

const SCRIPT: &str = "nostd.py";

/// The patterns of nostd.py, compiled once. The Python's module pattern has a lookahead
/// (`(?=\s*(?:::|,|\}|$))`), which this regex crate has not: `module` finds the candidates and
/// `after_module` is the lookahead, tried at each one's end (no candidate can start inside a
/// failed one, so the count is the same).
struct Patterns {
    direct: Regex,
    group: Regex,
    time_group: Regex,
    no_std: Regex,
    tests: Regex,
    extern_std: Regex,
    module: Regex,
    after_module: Regex,
    clock: Regex,
    time_clock: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| match Regex::new(pattern) {
            Ok(it) => it,
            Err(e) => fail(SCRIPT, &format!("a pattern of its own does not compile: {e}")),
        };
        Patterns {
            direct: re(r"\blibc::|\buse libc\b|\bstd::(?:fs|net|thread|process)\b|\bstd::time::(?:Instant|SystemTime)\b"),
            group: re(r"(?s)\bstd::\{([^;]*)\}"),
            time_group: re(r"\btime::\{([^{}]*)\}"),
            no_std: re(r"(?m)^#!\[(?:no_std|cfg_attr\(not\(test\), no_std\))\]"),
            tests: re(r"(?m)^#\[cfg\((?:all\()?test\b[^\]]*\]\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*mod \w+ \{"),
            extern_std: re(r"(?m)^\s*extern crate std\b"),
            module: re(r"(?:^|[{,\s])(?:fs|net|thread|process)\b"),
            after_module: re(r"\A\s*(?:::|,|\}|\z)"),
            clock: re(r"\b(?:Instant|SystemTime)\b"),
            time_clock: re(r"\btime::(?:Instant|SystemTime)\b"),
        }
    }
}

/// A file or directory that still names a banned path, and why it has not moved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    path: String,
    why: String,
}

impl Input for Listed {
    const FIELDS: &'static [&'static str] = &["path", "why"];

    fn parse(f: &Fields<'_>) -> Result<Listed, InputError> {
        Ok(Listed { path: f.str("path", None)?, why: f.str("why", None)? })
    }
}

/// nostd.json.
#[derive(Debug, Clone)]
struct Bans {
    sys: Vec<String>,
    no_std_crates: Vec<String>,
    no_std_packages: Vec<String>,
    not_yet_moved: Vec<Listed>,
}

impl Input for Bans {
    const FIELDS: &'static [&'static str] =
        &["about", "sys", "banned", "no_std_crates", "no_std_packages", "not_yet_moved"];

    fn parse(f: &Fields<'_>) -> Result<Bans, InputError> {
        // `about` and `banned` are read, as the Python's dataclass requires them, and used by
        // nothing.
        f.str("about", None)?;
        let sys = f.strs_required("sys")?;
        f.strs_required("banned")?;
        Ok(Bans {
            sys,
            no_std_crates: f.strs_required("no_std_crates")?,
            no_std_packages: f.strs_required("no_std_packages")?,
            not_yet_moved: f.objects("not_yet_moved", true)?,
        })
    }
}

struct Args {
    command: String,
    root: String,
    bans: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "bans"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "progress", "list"], None)?,
            root: f.str("root", Some("."))?,
            bans: f.str("bans", Some("harness/v2/nostd.json"))?,
        })
    }
}

/// `text` without its comments and without its tests: what a blaze binary links.
fn code_of(p: &Patterns, text: &str) -> String {
    let text = match p.tests.find(text) {
        Some(cut) => &text[..cut.start()],
        None => text,
    };
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| if lstrip(line).starts_with("//") { "" } else { line.split(" // ").next().unwrap_or(line) })
        .collect();
    lines.join("\n")
}

/// How many banned paths `text` names.
fn uses(p: &Patterns, text: &str) -> usize {
    let code = code_of(p, text);
    let mut found = p.direct.find_iter(&code).count();
    for group in p.group.captures_iter(&code) {
        let inner = group.get(1).map_or("", |m| m.as_str());
        // A module of the group, at its top level or nested: `fs`, `fs::File`, `fs::{..}`.
        found += p.module.find_iter(inner).filter(|m| p.after_module.is_match(&inner[m.end()..])).count();
        for clocks in p.time_group.captures_iter(inner) {
            found += p.clock.find_iter(clocks.get(1).map_or("", |m| m.as_str())).count();
        }
        found += p.time_clock.find_iter(inner).count();
    }
    found
}

fn is_test(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let Some((last, dirs)) = parts.split_last() else { return false };
    *last == "tests.rs" || ["tests", "benches", "examples"].iter().any(|it| dirs.contains(it))
}

/// `path` under `root`, `/` between its parts: `file.relative_to(root).as_posix()`.
fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// Every `.rs` path under `crates/`, sorted as Python sorts paths.
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut files = rglob(&root.join("crates"), |name| name.ends_with(".rs"));
    files.sort();
    files
}

fn lossy(file: &Path) -> String {
    read_text_lossy(file).unwrap_or_else(|e| fail(SCRIPT, &os_error(&e, &file.display().to_string())))
}

/// Every source file under `crates/` that names a banned path, with how many, in file order.
fn scan(p: &Patterns, root: &Path) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for file in sources(root) {
        let path = relative(&file, root);
        if is_test(&path) || path.contains("/target/") {
            continue;
        }
        let n = uses(p, &lossy(&file));
        if n > 0 {
            found.push((path, n));
        }
    }
    found
}

fn under(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
}

fn toml_of(path: &Path) -> toml::Table {
    let Some(text) = read_text(path) else { fail(SCRIPT, &format!("{} cannot be read as UTF-8 text", path.display())) };
    text.parse::<toml::Table>().unwrap_or_else(|e| fail(SCRIPT, &format!("{} is not TOML: {e}", path.display())))
}

/// Each crate of `crates/`, by package name, with its root source file.
fn crate_roots(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .map(|dir| dir.filter_map(Result::ok).map(|it| it.path().join("Cargo.toml")).filter(|it| it.is_file()).collect())
        .unwrap_or_default();
    manifests.sort();
    let mut roots = BTreeMap::new();
    for manifest in manifests {
        let dir = manifest.parent().unwrap_or(root);
        let package = toml_of(&manifest).get("package").and_then(toml::Value::as_table).cloned().unwrap_or_default();
        for candidate in ["src/lib.rs", "src/main.rs"] {
            let source = dir.join(candidate);
            if source.is_file() {
                let name = match package.get("name").and_then(toml::Value::as_str) {
                    Some(name) => name.to_owned(),
                    None => dir.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default(),
                };
                roots.insert(name, source);
                break;
            }
        }
    }
    roots
}

/// A package of the lock file: its name, whether it is a path package (blaze's), its
/// dependencies' names.
type Locked = (String, bool, Vec<String>);

/// The lock file's packages.
fn lock_packages(root: &Path) -> Vec<Locked> {
    let lock = toml_of(&root.join("Cargo.lock"));
    let packages = lock.get("package").and_then(toml::Value::as_array).cloned().unwrap_or_default();
    packages
        .iter()
        .map(|p| {
            let name = p.get("name").and_then(toml::Value::as_str).unwrap_or_else(|| fail(SCRIPT, "a package of Cargo.lock has no name"));
            let deps = p.get("dependencies").and_then(toml::Value::as_array).cloned().unwrap_or_default();
            let deps = deps.iter().filter_map(toml::Value::as_str).map(|d| d.split(' ').next().unwrap_or(d).to_owned()).collect();
            (name.to_owned(), p.get("source").is_none(), deps)
        })
        .collect()
}

/// The packages blaze's crates reach, through any chain, that are neither blaze's nor allowed
/// as `#![no_std]`: each may link std.
fn closure_outsiders(packages: &[Locked], allowed: &BTreeSet<String>) -> Vec<String> {
    // By name, the last of a name standing, as the Python's dict comprehension keeps it.
    let by_name: BTreeMap<&str, (bool, &[String])> =
        packages.iter().map(|(name, ours, deps)| (name.as_str(), (*ours, deps.as_slice()))).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = packages.iter().filter(|(_, ours, _)| *ours).map(|(name, _, _)| name.as_str()).collect();
    while let Some(name) = stack.pop() {
        let Some((_, deps)) = by_name.get(name) else { continue };
        if !seen.insert(name) {
            continue;
        }
        stack.extend(deps.iter().map(String::as_str));
    }
    seen.into_iter().filter(|name| !by_name[name].0 && !allowed.contains(*name)).map(str::to_owned).collect()
}

/// What is wrong: for `progress` only what went backwards or is stale; for `check` all of what
/// is left.
fn problems(
    bans: &Bans,
    found: &[(String, usize)],
    no_std: &BTreeMap<String, bool>,
    extern_std: &[String],
    outsiders: &[String],
    command: &str,
) -> Vec<String> {
    let mut said = Vec::new();
    let listed: Vec<&str> = bans.not_yet_moved.iter().map(|it| it.path.as_str()).collect();
    let in_sys = |path: &str| bans.sys.iter().any(|s| under(path, s));
    for (path, n) in found {
        if in_sys(path) {
            continue;
        }
        if !listed.iter().any(|l| under(path, l)) {
            said.push(format!(
                "{path}: names {n} banned path(s) and is not on the ban list: go through blaze-sys (or an effect), never around it"
            ));
        }
    }
    for l in &listed {
        if !found.iter().any(|(path, _)| under(path, l)) {
            said.push(format!("{l}: listed as not yet moved and names no banned path: take it off the list"));
        }
    }
    for name in &bans.no_std_crates {
        if !no_std.get(name).copied().unwrap_or(false) {
            said.push(format!("{name}: listed as `#![no_std]` and its root does not say so"));
        }
    }
    if command == "check" {
        let left: usize = found.iter().filter(|(path, _)| !in_sys(path)).map(|(_, n)| n).sum();
        if !listed.is_empty() {
            said.push(format!("the ban list is not empty: {} path(s), {left} banned use(s) left", listed.len()));
        }
        said.extend(no_std.iter().filter(|(_, ok)| !**ok).map(|(name, _)| format!("{name}: not `#![no_std]`")));
        said.extend(extern_std.iter().map(|path| format!("{path}: `extern crate std` outside tests")));
        said.extend(
            outsiders.iter().map(|name| format!("{name}: a package of the dependency closure that is not known to be `#![no_std]`")),
        );
    }
    said
}

fn main_with(p: &Patterns, it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let bans_path = root.join(&it.bans);
    let Some(bans_text) = read_text(&bans_path) else {
        fail(SCRIPT, &format!("{} cannot be read as UTF-8 text", bans_path.display()))
    };
    let bans: Bans = input::parse(&bans_text, &it.bans);
    let found = scan(p, &root);
    if it.command == "list" {
        for (path, n) in &found {
            println!("{n:5}  {path}");
        }
        println!("{:5}  in {} files", found.iter().map(|(_, n)| n).sum::<usize>(), found.len());
        return;
    }
    let no_std: BTreeMap<String, bool> = crate_roots(&root)
        .into_iter()
        .map(|(name, source)| {
            let Some(text) = read_text(&source) else { fail(SCRIPT, &format!("{} cannot be read as UTF-8 text", source.display())) };
            (name, p.no_std.is_match(&text))
        })
        .collect();
    let extern_std: Vec<String> = sources(&root)
        .into_iter()
        .filter(|file| !is_test(&relative(file, &root)) && p.extern_std.is_match(&code_of(p, &lossy(file))))
        .map(|file| relative(&file, &root))
        .collect();
    let allowed: BTreeSet<String> = bans.no_std_packages.iter().cloned().collect();
    let outsiders = closure_outsiders(&lock_packages(&root), &allowed);
    let said = problems(&bans, &found, &no_std, &extern_std, &outsiders, &it.command);
    // By row, a path listed twice counted once, at its first place, as the Python's dict keeps it.
    let mut by_row: Vec<(&str, usize)> = Vec::new();
    for l in &bans.not_yet_moved {
        let n = found.iter().filter(|(path, _)| under(path, &l.path)).map(|(_, n)| n).sum();
        match by_row.iter_mut().find(|(path, _)| *path == l.path) {
            Some(row) => row.1 = n,
            None => by_row.push((l.path.as_str(), n)),
        }
    }
    println!(
        "{SCRIPT}: {} path(s) on the ban list, {} banned use(s) in them; {} of {} crates are #![no_std]",
        by_row.len(),
        by_row.iter().map(|(_, n)| n).sum::<usize>(),
        no_std.values().filter(|ok| **ok).count(),
        no_std.len()
    );
    if it.command == "progress" {
        let mut rows = by_row.clone();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (path, n) in rows {
            println!("{n:5}  {path}");
        }
    }
    for line in said.iter().take(200) {
        println!("{SCRIPT}: {line}");
    }
    if said.len() > 200 {
        println!("{SCRIPT}: and {} more", said.len() - 200);
    }
    blaze_script::exit(i32::from(!said.is_empty()));
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|it| (*it).to_owned()).collect()
}

fn bans_of(sys: &[&str], no_std_crates: &[&str], no_std_packages: &[&str], not_yet_moved: &[(&str, &str)]) -> Bans {
    Bans {
        sys: strings(sys),
        no_std_crates: strings(no_std_crates),
        no_std_packages: strings(no_std_packages),
        not_yet_moved: not_yet_moved.iter().map(|(path, why)| Listed { path: (*path).to_owned(), why: (*why).to_owned() }).collect(),
    }
}

fn found_of(items: &[(&str, usize)]) -> Vec<(String, usize)> {
    items.iter().map(|(path, n)| ((*path).to_owned(), *n)).collect()
}

fn flags(items: &[(&str, bool)]) -> BTreeMap<String, bool> {
    items.iter().map(|(name, ok)| ((*name).to_owned(), *ok)).collect()
}

fn selftest(p: &Patterns) {
    let uses = |text: &str| uses(p, text);
    expect(uses("use std::fs;\nlet f = std::fs::read(p);\n") == 2, "two direct uses");
    expect(
        uses("use std::{\n    fs::{File, OpenOptions},\n    io,\n    thread::JoinHandle,\n    time::{Duration, Instant},\n};\n") == 3,
        "a group's fs, thread and Instant",
    );
    expect(uses("use std::{fmt, sync::Arc, time::Duration};\n") == 0, "fmt, Arc and Duration are not banned");
    expect(uses("use std::{net::IpAddr, time::Instant};\n") == 2, "net and time::Instant in a group");
    expect(uses("// std::fs::read is gone\n/// see `std::thread`\nfn f() {} // libc::open\n") == 0, "comments do not count");
    expect(uses("let e = libc::EINTR;\nuse libc::c_int;\n") == 2, "libc paths");
    expect(uses("fn f() {}\n#[cfg(test)]\nmod tests {\n    use std::fs;\n}\n") == 0, "tests do not count");
    expect(
        uses("#[cfg(test)]\nstruct OffThread;\nfn f() { std::thread::yield_now() }\n") == 1,
        "a test-only item does not hide the code after it",
    );
    expect(uses("use core::fmt;\nuse alloc::sync::Arc;\nlet s = mystd::fs::x;\n") == 0, "core, alloc and other names");
    expect(
        is_test("crates/a/src/tests.rs") && is_test("crates/a/tests/x.rs") && !is_test("crates/a/src/fs.rs"),
        "which files are tests",
    );
    expect(under("crates/a/src/x.rs", "crates/a") && !under("crates/ab/src/x.rs", "crates/a"), "path prefixes");
    expect(
        p.no_std.is_match("//! doc\n#![cfg_attr(not(test), no_std)]\n")
            && p.no_std.is_match("#![no_std]\n")
            && !p.no_std.is_match("// #![no_std]\n#![feature(x)]\n"),
        "the no_std attribute",
    );
    let bans = bans_of(&["crates/sys"], &["b"], &["cfg-if"], &[("crates/core/src/uring.rs", "S4 slice 2")]);
    let found = found_of(&[("crates/sys/src/lib.rs", 9), ("crates/core/src/uring.rs", 3)]);
    let none = problems(&bans, &found, &flags(&[("b", true), ("core", false)]), &[], &[], "progress");
    expect(none.is_empty(), &format!("a listed file and the system layer are no problem under way: {none:?}"));
    let mut more = found.clone();
    more.push(("crates/core/src/new.rs".to_owned(), 1));
    let back = problems(&bans, &more, &flags(&[("b", true)]), &[], &[], "progress");
    expect(
        back.len() == 1 && back[0].contains("crates/core/src/new.rs") && back[0].contains("not on the ban list"),
        &format!("an unlisted use is refused, naming the file: {back:?}"),
    );
    let stale = problems(&bans, &found_of(&[("crates/sys/src/lib.rs", 9)]), &flags(&[("b", true)]), &[], &[], "progress");
    expect(stale.len() == 1 && stale[0].contains("take it off the list"), &format!("a listed file with no use is stale: {stale:?}"));
    let lost = problems(&bans, &found, &flags(&[("b", false)]), &[], &[], "progress");
    expect(lost.len() == 1 && lost[0].contains("b: listed as `#![no_std]`"), &format!("a crate that lost no_std: {lost:?}"));
    let left = problems(
        &bans,
        &found,
        &flags(&[("b", true), ("core", false)]),
        &strings(&["crates/b/src/rt.rs"]),
        &strings(&["serde"]),
        "check",
    );
    for what in [
        "the ban list is not empty: 1 path(s), 3 banned use(s) left",
        "core: not `#![no_std]`",
        "crates/b/src/rt.rs: `extern crate std`",
        "serde: a package of the dependency closure",
    ] {
        expect(left.iter().any(|line| line.contains(what)), &format!("`{what}` is reported by check, of {left:?}"));
    }
    let done = problems(
        &bans_of(&["crates/sys"], &["b"], &["cfg-if"], &[]),
        &found_of(&[("crates/sys/src/lib.rs", 9)]),
        &flags(&[("b", true), ("sys", true)]),
        &[],
        &[],
        "check",
    );
    expect(done.is_empty(), &format!("an empty list, no_std crates and a known closure pass: {done:?}"));
    let package = |name: &str, ours: bool, deps: &[&str]| (name.to_owned(), ours, strings(deps));
    let packages = [
        package("a", true, &["b", "cfg-if"]),
        package("b", true, &["serde"]),
        package("serde", false, &["serde_core"]),
        package("serde_core", false, &[]),
        package("cfg-if", false, &[]),
        package("unreached", false, &[]),
    ];
    let allowed: BTreeSet<String> = [String::from("cfg-if")].into_iter().collect();
    let outsiders = closure_outsiders(&packages, &allowed);
    expect(outsiders == ["serde", "serde_core"], &format!("the closure's unknown packages: {outsiders:?}"));
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
            blaze_script::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Python's selftest, case for case, and every pattern compiles with the features the
    /// lock builds.
    #[test]
    fn selftest_passes() {
        selftest(&Patterns::new());
    }
}
