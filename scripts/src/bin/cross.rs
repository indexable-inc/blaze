//! cross: the check of ledger row X1 (blaze cross-compiles with nothing borrowed from the host),
//! and the generators of the symbol lists blaze-link ships for it. `DOC` below is what its usage
//! error prints.
//!
//!     blaze run -q -p scripts --bin cross -- '{"command": "check", "engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/cross.json"}'
//!     blaze run -q -p scripts --bin cross -- '{"command": "glibc-list", "abilists": "/tmp/glibc-abilists"}'
//!     blaze run -q -p scripts --bin cross -- '{"command": "darwin-list", "sdk": "/path/to/MacOSX.sdk"}'

const DOC: &str = r#"cross: blaze cross-compiles a program of Rust alone with nothing of the host (ledger row X1;
docs/v2/plan/design/toolchain-c.md, "Cross compilation").

    blaze run -q -p scripts --bin cross -- '{"command": "check", "engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/cross.json", "run": {"aarch64-unknown-linux-gnu": []}}'
    blaze run -q -p scripts --bin cross -- '{"command": "glibc-list", "abilists": "/tmp/glibc-abilists"}'
    blaze run -q -p scripts --bin cross -- '{"command": "darwin-list", "sdk": "/path/to/MacOSX.sdk"}'

The input is one JSON object:

    command   check        build harness/v2/cross/probe for each of `triples` with `blaze build
                           --target T` under an environment that has nothing of the machine
                           (`env -i`, a PATH of one empty directory, HOME and a BLAZE_HOME of its
                           own; no CC*, SDKROOT, LIBRARY_PATH or NIX_LDFLAGS), then run each binary
                           with the words `run` gives for its triple (an emulator, a remote
                           shell; none to run it here) and read its five checks. A triple with no
                           entry in `run` is built and reported `unmeasured`: the row is not met
                           until every triple ran. With `expect`, each binary's sha256 must be the
                           one given: the hashes another host printed, so two hosts' builds are
                           the same bytes
              glibc-list   write crates/blaze-link/src/target/shipped/glibc-<arch>.abilist from
                           `abilists`: a directory `<release>/<arch>/<library>.abilist` of glibc's
                           own files at each release tag (sysdeps/unix/sysv/linux/**), with
                           `commits.json` naming each tag's commit. With `"write": false` (the
                           default) it compares and fails on a difference
              darwin-list  write crates/blaze-link/src/target/shipped/libSystem.tbd: every name
                           library/ declares (`fn`, `static`, `link_name`) that `sdk`'s
                           usr/lib/libSystem.B.tbd exports for arm64 macOS, and `extras` below.
                           No text of the SDK is copied: the SDK says which declared names exist
    engine    `check`: the blaze measured (engine.EngineSpec)
    out       `check`: the result's path (report.rs's JSON); empty for none
    root      the checkout (".")
    triples   `check`: the targets; the five of the row when empty
    run       `check`: per triple, the words put before the binary's path to run it
    expect    `check`: per triple, the sha256 its binary must have
    work      `check`: where the build's home goes; a new directory under /tmp when empty
    abilists, sdk, write   as above
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    Value,
    engine::EngineSpec,
    fail,
    input::{self, Fields, Input, InputError},
    read_text, read_text_lossy,
    regex::Regex,
    report::{Report, Verdict},
    run,
};

const SCRIPT: &str = "cross";

/// The targets of row X1. The musl ones are refused by blaze-link today, and say so.
const TRIPLES: [&str; 5] = [
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
    "aarch64-apple-darwin",
];

/// What the probe prints last when its five checks passed (harness/v2/cross/probe).
const PASSED: &str = "cross-probe: 5 of 5";

const SHIPPED: &str = "crates/blaze-link/src/target/shipped";

/// Mach-O names the linker or the compiler refers to that no Rust source declares.
const DARWIN_EXTRAS: [&str; 15] = [
    "dyld_stub_binder",
    "__tlv_bootstrap",
    "___stack_chk_fail",
    "___stack_chk_guard",
    "___error",
    "__NSGetArgc",
    "__NSGetArgv",
    "__NSGetEnviron",
    "__NSGetExecutablePath",
    "___chkstk_darwin",
    "___bzero",
    "__dyld_get_image_header",
    "__dyld_get_image_vmaddr_slide",
    "__dyld_image_count",
    "__dyld_get_image_name",
];

/// glibc's libraries blaze ships a list of: the `.abilist`'s stem and the shared object's name
/// (the loader's is per architecture).
const GLIBC_LIBS: [(&str, &str); 8] = [
    ("libc", "libc.so.6"),
    ("libm", "libm.so.6"),
    ("libpthread", "libpthread.so.0"),
    ("libdl", "libdl.so.2"),
    ("librt", "librt.so.1"),
    ("libutil", "libutil.so.1"),
    ("libresolv", "libresolv.so.2"),
    ("ld", ""),
];

const GLIBC_ARCHES: [(&str, &str); 2] =
    [("x86_64", "ld-linux-x86-64.so.2"), ("aarch64", "ld-linux-aarch64.so.1")];

struct Args {
    command: String,
    engine: Option<EngineSpec>,
    out: String,
    root: String,
    triples: Vec<String>,
    run: Value,
    expect: Value,
    work: String,
    abilists: String,
    sdk: String,
    write: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &[
        "command", "engine", "out", "root", "triples", "run", "expect", "work", "abilists", "sdk",
        "write",
    ];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "glibc-list", "darwin-list"], None)?,
            engine: f.object("engine", false)?,
            out: f.str("out", Some(""))?,
            root: f.str("root", Some("."))?,
            triples: f.strs("triples")?,
            run: f.any("run"),
            expect: f.any("expect"),
            work: f.str("work", Some(""))?,
            abilists: f.str("abilists", Some(""))?,
            sdk: f.str("sdk", Some(""))?,
            write: f.bool("write", Some(false))?,
        })
    }
}

fn text_of(path: &Path) -> String {
    match read_text(path) {
        Some(it) => it,
        None => fail(SCRIPT, &format!("{} cannot be read as UTF-8 text", path.display())),
    }
}

/// Writes `text` to `path`, or with `write` false fails when `path` holds anything else.
fn settle(path: &Path, text: &str, write: bool) {
    if write {
        if let Err(e) = std::fs::write(path, text) {
            fail(SCRIPT, &format!("{}: {e}", path.display()));
        }
        println!("{SCRIPT}: wrote {} ({} bytes)", path.display(), text.len());
        return;
    }
    if read_text(path).as_deref() != Some(text) {
        fail(
            SCRIPT,
            &format!("{} is not what its sources generate: run again with \"write\": true", path.display()),
        );
    }
    println!("{SCRIPT}: {} is what its sources generate", path.display());
}

/// A release or symbol version's numbers, which order it (`2.2.5` after `2.2`, before `2.3`).
fn numbers(version: &str) -> Vec<u32> {
    version.split('.').map(|it| it.parse().unwrap_or(0)).collect()
}

/// The `(name, version, kind)` rows of one `.abilist`, in any of glibc's three layouts: a
/// version on a line of its own with its symbols indented under it (2.17), or `VERSION name
/// KIND [size]` on each line. `A` rows name the version itself and are left out.
fn abilist(path: &Path) -> BTreeSet<(String, String, String)> {
    let mut rows = BTreeSet::new();
    let mut group = String::new();
    for line in text_of(path).lines().filter(|it| !it.trim().is_empty()) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let (version, name, kind) = match (line.starts_with(' '), parts.as_slice()) {
            (false, [version]) => {
                group = (*version).to_owned();
                continue;
            }
            (false, [version, name, kind, ..]) => (*version, *name, *kind),
            (true, [name, kind, ..]) => (group.as_str(), *name, *kind),
            _ => fail(SCRIPT, &format!("{}: a line that is not an abilist's: `{line}`", path.display())),
        };
        if kind == "A" {
            continue;
        }
        if !matches!(kind, "F" | "D" | "T") || !version.starts_with("GLIBC_2") {
            fail(SCRIPT, &format!("{}: `{line}`: a kind or version this does not know", path.display()));
        }
        rows.insert((name.to_owned(), version["GLIBC_".len()..].to_owned(), kind.to_owned()));
    }
    rows
}

/// One architecture's list: for each library, each `(symbol, kind, version)` with the first
/// release that has it and, when it has left, the last. A row present in separated runs of
/// releases is a line per run.
fn glibc_list(dir: &Path, arch: &str, loader: &str, releases: &[String]) -> String {
    let mut out = String::new();
    let (first, last) = (&releases[0], &releases[releases.len() - 1]);
    out.push_str(&format!(
        "# glibc's exported symbols by release, {first} to {last}, for {arch}-unknown-linux-gnu.\n\
         # Generated by `scripts --bin cross` (glibc-list) from glibc's own sysdeps/unix/sysv/linux/**/*.abilist\n\
         # at each release tag of https://sourceware.org/git/glibc.git; symbol names and versions only.\n\
         # glibc is LGPL-2.1-or-later; this is a list of facts about its ABI and holds none of its code.\n\
         # A line: symbol, kind (F function, D data, T thread-local), version, first release that has it\n\
         # under that version in this library, and the last when it has left.\n\
         releases {}\n",
        releases.join(" ")
    ));
    for (stem, soname) in GLIBC_LIBS {
        let soname = if soname.is_empty() { loader } else { soname };
        let mut present: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
        for (at, release) in releases.iter().enumerate() {
            let path = dir.join(release).join(arch).join(format!("{stem}.abilist"));
            for row in abilist(&path) {
                present.entry(row).or_default().push(at);
            }
        }
        out.push_str(&format!("library {soname}\n"));
        let mut rows: Vec<(String, Vec<u32>, String, String, usize, usize)> = Vec::new();
        for ((name, version, kind), ats) in present {
            if name.ends_with("_version_placeholder") {
                continue;
            }
            let (mut start, mut prev) = (ats[0], ats[0]);
            for at in ats.iter().skip(1).copied().chain([usize::MAX]) {
                if at != prev.wrapping_add(1) {
                    rows.push((name.clone(), numbers(&version), kind.clone(), version.clone(), start, prev));
                    start = at;
                }
                prev = at;
            }
        }
        rows.sort();
        for (name, _, kind, version, start, end) in rows {
            match end == releases.len() - 1 {
                true => out.push_str(&format!("{name} {kind} {version} {}\n", releases[start])),
                false => out.push_str(&format!("{name} {kind} {version} {} {}\n", releases[start], releases[end])),
            }
        }
    }
    out
}

fn glibc_lists(root: &Path, it: &Args) {
    if it.abilists.is_empty() {
        fail(SCRIPT, "glibc-list needs `abilists`, the directory of glibc's .abilist files by release");
    }
    let dir = Path::new(&it.abilists);
    let commits: Value = match blaze_script::json::from_str(&text_of(&dir.join("commits.json"))) {
        Ok(it) => it,
        Err(e) => fail(SCRIPT, &format!("{}/commits.json: {e}", dir.display())),
    };
    let Some(commits) = commits.as_object() else {
        fail(SCRIPT, "commits.json is not an object of release to commit")
    };
    let mut releases: Vec<String> = commits.keys().cloned().collect();
    releases.sort_by_key(|it| numbers(it));
    if releases.is_empty() {
        fail(SCRIPT, "commits.json names no release");
    }
    for (arch, loader) in GLIBC_ARCHES {
        let text = glibc_list(dir, arch, loader, &releases);
        settle(&root.join(SHIPPED).join(format!("glibc-{arch}.abilist")), &text, it.write);
    }
}

/// The names libSystem exports for arm64 macOS, by the kind of list they are in (`symbols`,
/// `weak-symbols`, `thread-local-symbols`): every `- targets: [...]` entry of every document's
/// `exports` and `reexports` that lists `arm64-macos` or `arm64e-macos`.
fn sdk_exports(text: &str) -> BTreeMap<&'static str, BTreeSet<String>> {
    const KINDS: [&str; 3] = ["symbols", "weak-symbols", "thread-local-symbols"];
    let mut out: BTreeMap<&'static str, BTreeSet<String>> = KINDS.iter().map(|it| (*it, BTreeSet::new())).collect();
    let (mut section, mut selected) = (String::new(), false);
    let lines: Vec<&str> = text.lines().collect();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        at += 1;
        if line.starts_with("--- ") {
            (section, selected) = (String::new(), false);
            continue;
        }
        if line.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && let Some((key, _)) = line.split_once(':')
        {
            section = key.to_owned();
        }
        if section != "exports" && section != "reexports" {
            continue;
        }
        let entry = line.trim_start().trim_start_matches("- ");
        let Some((key, rest)) = entry.split_once(':') else { continue };
        if key != "targets" && !KINDS.contains(&key) {
            continue;
        }
        // A flow list may wrap across lines.
        let mut list = rest.to_owned();
        while !list.contains(']') && at < lines.len() {
            list.push(' ');
            list.push_str(lines[at].trim());
            at += 1;
        }
        let inner = list.trim().trim_start_matches('[').trim_end_matches(']');
        let items = inner.split(',').map(|it| it.trim().trim_matches(['\'', '"'])).filter(|it| !it.is_empty());
        match key {
            "targets" => selected = items.into_iter().any(|it| it == "arm64-macos" || it == "arm64e-macos"),
            _ if selected => {
                let kind = KINDS.iter().find(|it| **it == key).copied().unwrap_or(KINDS[0]);
                out.entry(kind).or_default().extend(items.map(str::to_owned));
            }
            _ => {}
        }
    }
    out
}

/// A `.tbd` flow list of `items`, wrapped as tapi wraps them.
fn flow(items: &[String]) -> String {
    let (mut lines, mut current): (Vec<String>, String) = (Vec::new(), String::new());
    for item in items {
        let item = if item.contains('$') { format!("'{item}'") } else { item.clone() };
        if current.len() + item.len() + 2 > 96 {
            lines.push(current.trim_end().to_owned());
            current = String::new();
        }
        current.push_str(&item);
        current.push_str(", ");
    }
    lines.push(current.trim_end().trim_end_matches(',').to_owned());
    lines.join(&format!("\n{}", " ".repeat(23)))
}

fn darwin_list(root: &Path, it: &Args) {
    if it.sdk.is_empty() {
        fail(SCRIPT, "darwin-list needs `sdk`, the macOS SDK to check names against");
    }
    let exported = sdk_exports(&text_of(&Path::new(&it.sdk).join("usr/lib/libSystem.B.tbd")));
    let declares = match Regex::new(r#"\b(?:fn|static(?:\s+mut)?)\s+([A-Za-z_][A-Za-z0-9_]*)|link_name\s*=\s*"([^"]+)""#) {
        Ok(it) => it,
        Err(e) => fail(SCRIPT, &format!("a pattern of its own does not compile: {e}")),
    };
    let mut names: BTreeSet<String> = DARWIN_EXTRAS.iter().map(|it| (*it).to_owned()).collect();
    let mut sources = blaze_script::pypath::rglob(&root.join("library"), |name| name.ends_with(".rs"));
    sources.sort();
    for source in sources {
        let Ok(text) = read_text_lossy(&source) else { continue };
        for found in declares.captures_iter(&text) {
            if let Some(name) = found.get(1).or_else(|| found.get(2)) {
                names.insert(format!("_{}", name.as_str()));
            }
        }
    }
    let all: BTreeSet<&String> = exported.values().flatten().collect();
    for extra in DARWIN_EXTRAS {
        if !all.contains(&extra.to_owned()) {
            fail(SCRIPT, &format!("the SDK's libSystem does not export `{extra}`, which this always lists"));
        }
    }
    let mut text = String::from(
        "--- !tapi-tbd\ntbd-version:     4\ntargets:         [ arm64-macos ]\n\
         install-name:    '/usr/lib/libSystem.B.dylib'\nexports:\n  - targets:         [ arm64-macos ]\n",
    );
    for kind in ["symbols", "weak-symbols", "thread-local-symbols"] {
        let picked: Vec<String> =
            exported.get(kind).map(|it| it.intersection(&names).cloned().collect()).unwrap_or_default();
        if !picked.is_empty() {
            text.push_str(&format!("    {:<16} [ {} ]\n", format!("{kind}:"), flow(&picked)));
        }
    }
    text.push_str("...\n");
    settle(&root.join(SHIPPED).join("libSystem.tbd"), &text, it.write);
}

/// The sha256 of `path`, by the machine's `sha256sum` or `shasum -a 256`.
fn sha256(path: &Path, cwd: &Path) -> Result<String, String> {
    let file = path.display().to_string();
    let argv: Vec<&str> = match blaze_script::which("sha256sum") {
        Some(_) => vec!["sha256sum", &file],
        None => vec!["shasum", "-a", "256", &file],
    };
    let ran = run(&argv, cwd)?;
    match ran.out().split_whitespace().next() {
        Some(hash) if ran.status == 0 && hash.len() == 64 => Ok(hash.to_owned()),
        _ => Err(format!("{} failed: {}", argv.join(" "), ran.err().trim())),
    }
}

/// The file named `name` under `dir` whose path has the component `triple`.
fn built(dir: &Path, triple: &str, name: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = blaze_script::pypath::rglob(dir, |it| it == name)
        .into_iter()
        .filter(|it| it.is_file() && it.components().any(|part| part.as_os_str() == triple))
        .collect();
    found.sort();
    found.into_iter().next()
}

fn words(value: &Value, triple: &str) -> Option<Vec<String>> {
    let list = value.as_object()?.get(triple)?.as_array()?;
    Some(list.iter().filter_map(|it| it.as_str().map(str::to_owned)).collect())
}

fn check(root: &Path, it: &Args) -> ! {
    let Some(engine) = &it.engine else { fail(SCRIPT, "check needs `engine`, the blaze measured") };
    let mut report = Report::new(SCRIPT, engine, &it.out);
    let work = match it.work.is_empty() {
        true => PathBuf::from(format!("/tmp/cross-{}", std::process::id())),
        false => PathBuf::from(&it.work),
    };
    let (home, empty) = (work.join("home"), work.join("empty"));
    for dir in [&home, &empty] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            fail(SCRIPT, &format!("{}: {e}", dir.display()));
        }
    }
    let blaze = std::fs::canonicalize(&engine.blaze).unwrap_or_else(|_| PathBuf::from(&engine.blaze));
    let probe = root.join("harness/v2/cross/probe");
    let triples: Vec<String> = match it.triples.is_empty() {
        true => TRIPLES.iter().map(|it| (*it).to_owned()).collect(),
        false => it.triples.clone(),
    };
    for triple in &triples {
        // Nothing of the machine: `env -i`, a PATH of one empty directory.
        let (path, user_home, blaze_home) = (
            format!("PATH={}", empty.display()),
            format!("HOME={}", work.display()),
            format!("BLAZE_HOME={}", home.display()),
        );
        let program = blaze.display().to_string();
        let argv = ["/usr/bin/env", "-i", &path, &user_home, &blaze_home, &program, "build", "--target", triple];
        let ran = match run(&argv, &probe) {
            Ok(ran) => ran,
            Err(e) => {
                report.add(&format!("build {triple}"), Verdict::Error, &e, vec![]);
                continue;
            }
        };
        if ran.status != 0 {
            let said = ran.err();
            let tail: Vec<&str> = said.lines().filter(|it| it.starts_with("error")).take(3).collect();
            report.add(&format!("build {triple}"), Verdict::Fail, &tail.join(" | "), vec![]);
            continue;
        }
        let Some(binary) = built(&home, triple, "cross-probe") else {
            report.add(&format!("build {triple}"), Verdict::Error, "the build left no cross-probe", vec![]);
            continue;
        };
        let hash = match sha256(&binary, &probe) {
            Ok(hash) => hash,
            Err(e) => {
                report.add(&format!("build {triple}"), Verdict::Error, &e, vec![]);
                continue;
            }
        };
        report.add(&format!("build {triple}"), Verdict::Pass, &format!("sha256 {hash}"), vec![]);
        if let Some(expected) = it.expect.as_object().and_then(|all| all.get(triple.as_str())).and_then(Value::as_str) {
            let verdict = if expected == hash { Verdict::Pass } else { Verdict::Fail };
            report.add(&format!("same bytes {triple}"), verdict, &format!("the other host's is {expected}"), vec![]);
        }
        let Some(before) = words(&it.run, triple) else {
            report.add(&format!("run {triple}"), Verdict::Unmeasured, "no way to run it was given (`run`)", vec![]);
            continue;
        };
        let file = binary.display().to_string();
        let mut argv: Vec<&str> = before.iter().map(String::as_str).collect();
        argv.push(&file);
        match run(&argv, &probe) {
            Ok(ran) if ran.status == 0 && ran.out().contains(PASSED) => {
                report.add(&format!("run {triple}"), Verdict::Pass, PASSED, vec![]);
            }
            Ok(ran) => {
                let said = format!("exit {}: {} {}", ran.status, ran.out().trim(), ran.err().trim());
                report.add(&format!("run {triple}"), Verdict::Fail, blaze_script::engine::last_chars(&said, 600), vec![]);
            }
            Err(e) => {
                report.add(&format!("run {triple}"), Verdict::Error, &e, vec![]);
            }
        }
    }
    report.finish()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [one] = args.as_slice() else {
        eprintln!("usage: {SCRIPT} '<json>'\n\n{DOC}");
        blaze_script::exit(1);
    };
    let it: Args = input::parse(one, SCRIPT);
    let root = std::fs::canonicalize(&it.root).unwrap_or_else(|_| PathBuf::from(&it.root));
    match it.command.as_str() {
        "check" => check(&root, &it),
        "glibc-list" => glibc_lists(&root, &it),
        _ => darwin_list(&root, &it),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_by_their_numbers() {
        assert!(numbers("2.2.5") < numbers("2.3"));
        assert!(numbers("2.9") < numbers("2.17"));
    }

    #[test]
    fn the_sdks_exports_are_read_for_arm64_macos_alone() {
        let text = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ x86_64-macos, arm64e-macos ]\ninstall-name: '/usr/lib/a.dylib'\nexports:\n  - targets: [ x86_64-macos ]\n    symbols: [ _only_x86 ]\n  - targets: [ x86_64-macos, arm64e-macos ]\n    symbols: [ _a, '_b$c',\n               _d ]\n    weak-symbols: [ _w ]\n...\n";
        let got = sdk_exports(text);
        let symbols: Vec<&str> = got["symbols"].iter().map(String::as_str).collect();
        assert_eq!(symbols, ["_a", "_b$c", "_d"]);
        assert!(got["weak-symbols"].contains("_w"));
    }
}
