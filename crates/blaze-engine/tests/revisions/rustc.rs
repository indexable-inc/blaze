//! rustc's own incremental tests (rust-lang/rust, `tests/incremental`) through the real
//! driver: the importer of compiletest's single-file form.
//!
//! A test there is one file with `//@ revisions: rpass1 rpass2 ..`; compiletest compiles it
//! once per revision with `--cfg <revision>` on one incremental directory, and the source
//! changes between revisions by `#[cfg(rpass1)]` / `#[cfg(rpass2)]` on its items. Here the
//! file is a package's root and a revision is the package's `revision` file rewritten, which
//! a build script the importer writes reads and gives back as `cargo::rustc-cfg=<revision>`:
//! the one edit between two builds on one daemon, and a cfg of that package alone (a
//! `--cfg` in `build.rustflags` re-keys the standard library's units too, 147 of them, so it
//! would test no reuse). A revision's prefix says what must come of it: `cpass` checks,
//! `bpass` builds, `bfail` and `cfail` fail to, `rpass` and `rfail` build.
//!
//! One thing in a file is changed: each `#[rustc_clean(..)]`, `#![rustc_partition_reused(..)]`,
//! `#![rustc_partition_codegened(..)]`, `#![rustc_expected_cgu_reuse(..)]`,
//! `#[rustc_if_this_changed(..)]` and `#[rustc_then_this_would_need(..)]` is written over
//! with spaces, lines and columns kept. blaze refuses them as rustc does without
//! `-Z query-dep-graph` ("attribute requires -Z query-dep-graph to be enabled") and refuses
//! the flag ("blaze does not know which cfgs `-Z query-dep-graph` decides"), so 88 of the
//! corpus's files build no other way. `BLAZE_REVISIONS_RUSTC_ATTRS=keep` leaves them in.
//!
//! The corpus is untrusted data, fetched read-only into a directory outside the tree and
//! named by `BLAZE_REVISIONS_RUSTC`. Nothing in it is executed: its sources are compiled by
//! blaze, its proc macros run in blaze's wasm sandbox as any macro does, and an `rpass`
//! program is built and not run (which a sandboxed `blaze run` would lift).
//!
//! `#[rustc_clean]` and its family name rustc's queries and are not held; each
//! `#[rustc_clean(cfg = "<revision>")]` on a function with no `except` is read as a hint
//! that the function is unchanged in that revision, and the report counts those blaze made
//! again.

use std::{
    collections::BTreeMap,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
};

use super::{
    real::{self, Clean},
    spec::{Action, Change, Expect, Revision, Step, Test},
};

/// A test's directives: each `//@ name: value` line (and `//@[revision] name: value`).
struct Directives {
    all: Vec<(Option<String>, String, String)>,
}

impl Directives {
    fn of(source: &str) -> Directives {
        let mut all = Vec::new();
        for line in source.lines() {
            let Some(rest) = line.trim_start().strip_prefix("//@") else { continue };
            let rest = rest.trim_start();
            let (revision, rest) = match rest.strip_prefix('[').and_then(|it| it.split_once(']')) {
                Some((revision, rest)) => (Some(revision.to_owned()), rest.trim_start()),
                None => (None, rest),
            };
            let (name, value) = match rest.split_once(':') {
                Some((name, value)) => (name.trim(), value.trim()),
                None => (rest.trim(), ""),
            };
            all.push((revision, name.to_owned(), value.to_owned()));
        }
        Directives { all }
    }

    fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.all.iter().filter(move |it| it.1 == name).map(|it| it.2.as_str())
    }
}

/// Why the importer does not run a test, or the test as a [`Test`] with its hints.
pub enum Imported {
    Skipped(String),
    Test(Box<Test>, Vec<(String, String)>),
}

/// The flags of rustc's own incremental machinery, which name nothing blaze has.
const RUSTC_ONLY: &[&str] = &["query-dep-graph", "incremental-ignore-spans", "incremental-verify-ich", "human-readable-cgu-names"];

/// `flags` as compiletest splits them, sorted into: what the manifest says (`lib`, the opt
/// level, and `profile`'s lines: blaze takes `panic`, `overflow-checks`, `debug-assertions` and
/// `opt-level` from the profile and refuses them in rustflags), what goes to `rustflags`, and
/// the first the importer has no place for.
fn flags(text: &str, lib: &mut bool, opt: &mut bool, pass: &mut Vec<String>, profile: &mut Vec<String>) -> Result<(), String> {
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let mut joined = |flag: &str| -> String {
            match word.strip_prefix(flag) {
                Some("") => words.next().unwrap_or("").to_owned(),
                Some(rest) => rest.trim_start_matches('=').to_owned(),
                None => String::new(),
            }
        };
        if word.starts_with("-Z") {
            let value = joined("-Z");
            let name = value.split('=').next().unwrap_or("");
            if !RUSTC_ONLY.contains(&name) {
                return Err(format!("-Z {name}"));
            }
        } else if word.starts_with("--crate-type") {
            match joined("--crate-type").as_str() {
                "lib" | "rlib" => *lib = true,
                other => return Err(format!("--crate-type {other}")),
            }
        } else if word == "-O" {
            *opt = true;
        } else if word == "-g" {
            // The dev profile has debug info already.
        } else if word.starts_with("--cfg") {
            pass.push("--cfg".to_owned());
            pass.push(joined("--cfg"));
        } else if word.starts_with("-C") {
            let value = joined("-C");
            let (name, set) = value.split_once('=').unwrap_or((value.as_str(), "yes"));
            let on = matches!(set, "yes" | "on" | "true" | "y");
            match name {
                "overflow-checks" | "debug-assertions" => profile.push(format!("{name} = {on}")),
                "opt-level" => profile.push(format!("opt-level = {set:?}").replace('"', "")),
                "panic" => profile.push(format!("panic = {set:?}")),
                other => return Err(format!("-C {other}")),
            }
        } else {
            return Err(word.to_owned());
        }
    }
    Ok(())
}

fn crate_name(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().replace('-', "_")
}

/// The attributes that assert on rustc's dependency graph, which name its queries and
/// its codegen units.
const RUSTC_ASSERTS: &[&str] = &[
    "rustc_clean",
    "rustc_partition_reused",
    "rustc_partition_codegened",
    "rustc_expected_cgu_reuse",
    "rustc_if_this_changed",
    "rustc_then_this_would_need",
];

/// `source` with each of [`RUSTC_ASSERTS`] written over with spaces, from its `#` to the `]`
/// that closes it: every other byte, and every line and column, as it was.
fn blanked(source: &str) -> String {
    let mut out = source.as_bytes().to_vec();
    let mut at = 0;
    while let Some(found) = source[at..].find('#') {
        let start = at + found;
        let rest = source[start + 1..].trim_start_matches('!');
        at = start + 1;
        let Some(inner) = rest.strip_prefix('[') else { continue };
        if !RUSTC_ASSERTS.iter().any(|name| inner.strip_prefix(name).is_some_and(|it| it.starts_with(['(', ']']))) {
            continue;
        }
        let mut depth = 0usize;
        for (offset, byte) in source.as_bytes()[start..].iter().enumerate() {
            match byte {
                b'[' => depth += 1,
                b']' => depth -= 1,
                _ => {}
            }
            if *byte != b'\n' {
                out[start + offset] = b' ';
            }
            if *byte == b']' && depth == 0 {
                at = start + offset + 1;
                break;
            }
        }
    }
    String::from_utf8(out).expect("ASCII written over ASCII")
}

/// The build script of every package the importer writes: the `revision` file's word as a cfg.
const SCRIPT: &str = "fn main() {
    println!(\"cargo::rerun-if-changed=revision\");
    let revision = std::fs::read_to_string(\"revision\").expect(\"the revision file\");
    println!(\"cargo::rustc-cfg={}\", revision.trim());
}
";

/// The functions `source` marks `#[rustc_clean(cfg = "<revision>")]` with no `except`: each
/// as (revision, function name).
fn clean_hints(source: &str) -> Vec<(String, String)> {
    let mut hints = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if let Some(at) = line.find("#[rustc_clean(") {
            let args = &line[at..];
            if !args.contains("except") {
                if let Some(cfg) = args.split("cfg").nth(1).and_then(|it| it.split('"').nth(1)) {
                    pending.push(cfg.to_owned());
                }
            }
            continue;
        }
        if line.starts_with("#[") || line.starts_with("//") || line.is_empty() {
            continue;
        }
        if let Some(name) = line.split("fn ").nth(1).filter(|_| !pending.is_empty()) {
            let name: String = name.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            hints.extend(pending.drain(..).map(|cfg| (cfg, name.clone()))); // clone: one name a hint
        }
        pending.clear();
    }
    hints
}

/// The test of `file`, a `.rs` file of rustc's `tests/incremental`.
pub fn import(file: &Path, name: &str) -> Imported {
    let Ok(source) = fs::read_to_string(file) else {
        return Imported::Skipped("the file is not UTF-8".to_owned());
    };
    let directives = Directives::of(&source);
    let Some(revisions) = directives.values("revisions").next() else {
        return Imported::Skipped("no `//@ revisions:`".to_owned());
    };
    let revisions: Vec<String> = revisions.split_whitespace().map(str::to_owned).collect();
    let (mut lib, mut opt, mut pass, mut profile) = (false, false, Vec::new(), Vec::new());
    let mut edition = "2015".to_owned();
    let mut aux: Vec<(String, bool)> = Vec::new();
    for (revision, directive, value) in &directives.all {
        let unsupported = |what: &str| Imported::Skipped(format!("the importer has no place for `{what}`"));
        match directive.as_str() {
            "revisions" | "ignore-backends" | "no-prefer-dynamic" | "ignore-windows" | "needs-unwind" | "needs-asm-support"
            | "needs-crate-type" | "dont-check-compiler-stderr" | "dont-check-compiler-stdout" => {}
            "edition" => edition = value.clone(), // clone: the directive's value, kept
            "aux-build" => aux.push((value.clone(), false)), // clone: as above
            "proc-macro" => aux.push((value.clone(), true)), // clone: as above
            "compile-flags" if revision.is_none() => {
                if let Err(flag) = flags(value, &mut lib, &mut opt, &mut pass, &mut profile) {
                    return unsupported(&format!("compile-flags: {flag}"));
                }
            }
            // One revision's flags: only those that name rustc's own incremental machinery
            // (and `-g`, which the dev profile has), which are no change of the build here.
            "compile-flags" => {
                let (mut lib, mut opt, mut pass, mut profile) = (false, false, Vec::new(), Vec::new());
                let taken = flags(value, &mut lib, &mut opt, &mut pass, &mut profile);
                if let Err(flag) = taken {
                    return unsupported(&format!("a revision's compile-flags: {flag}"));
                }
                if lib || opt || !pass.is_empty() || !profile.is_empty() {
                    return unsupported("a revision's compile-flags that change the build");
                }
            }
            other if other.starts_with("only-") && !other.contains("aarch64") && !other.contains("apple") && !other.contains("macos") && !cfg!(target_arch = "x86_64") => {
                return Imported::Skipped(format!("`{other}`: not this host"));
            }
            other if other.starts_with("only-") => {}
            other => return unsupported(&match revision {
                Some(revision) => format!("[{revision}] {other}"),
                None => other.to_owned(),
            }),
        }
    }
    for attribute in ["crate_type = \"rlib\"", "crate_type = \"lib\"", "crate_type=\"rlib\"", "crate_type=\"lib\""] {
        lib |= source.contains(attribute);
    }
    for other in ["crate_type = \"cdylib\"", "crate_type = \"dylib\"", "crate_type = \"staticlib\"", "crate_type = \"proc-macro\""] {
        if source.contains(other) {
            return Imported::Skipped(format!("the importer has no place for `#![{other}]`"));
        }
    }
    let package = crate_name(file);
    let mut manifest = format!("[package]\nname = \"{package}\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\n\n");
    let root = match lib {
        true => "src/lib.rs",
        false => "src/main.rs",
    };
    let keep = std::env::var("BLAZE_REVISIONS_RUSTC_ATTRS").is_ok_and(|it| it == "keep");
    let written = |source: &str| match keep {
        true => source.to_owned(),
        false => blanked(source),
    };
    let mut files = vec![
        Change { path: root.to_owned(), bytes: Some(written(&source).into_bytes()) },
        Change { path: "build.rs".to_owned(), bytes: Some(SCRIPT.as_bytes().to_vec()) },
    ];
    let mut revision_files = vec!["revision".to_owned()];
    if !aux.is_empty() {
        manifest.push_str("[dependencies]\n");
    }
    for (aux_file, proc_macro) in &aux {
        let path = file.parent().unwrap().join("auxiliary").join(aux_file);
        let Ok(aux_source) = fs::read_to_string(&path) else {
            return Imported::Skipped(format!("its auxiliary file {aux_file} is not there"));
        };
        let aux_directives = Directives::of(&aux_source);
        if let Some((_, other, _)) = aux_directives.all.iter().find(|it| !["revisions", "compile-flags", "no-prefer-dynamic", "edition", "ignore-backends"].contains(&it.1.as_str())) {
            return Imported::Skipped(format!("the importer has no place for `{other}` in an auxiliary file"));
        }
        let (mut aux_lib, mut aux_opt, mut aux_pass, mut aux_profile) = (true, false, Vec::new(), Vec::new());
        for value in aux_directives.values("compile-flags") {
            if let Err(flag) = flags(value, &mut aux_lib, &mut aux_opt, &mut aux_pass, &mut aux_profile) {
                return Imported::Skipped(format!("the importer has no place for `compile-flags: {flag}` in an auxiliary file"));
            }
        }
        let aux_edition = aux_directives.values("edition").next().unwrap_or("2015");
        let aux_name = crate_name(&path);
        manifest.push_str(&format!("{aux_name} = {{ path = \"aux/{aux_name}\" }}\n"));
        let kind = match proc_macro {
            true => "\n[lib]\nproc-macro = true\n",
            false => "",
        };
        files.push(Change {
            path: format!("aux/{aux_name}/Cargo.toml"),
            bytes: Some(format!("[package]\nname = \"{aux_name}\"\nversion = \"0.0.0\"\nedition = \"{aux_edition}\"\n{kind}").into_bytes()),
        });
        files.push(Change { path: format!("aux/{aux_name}/src/lib.rs"), bytes: Some(written(&aux_source).into_bytes()) });
        files.push(Change { path: format!("aux/{aux_name}/build.rs"), bytes: Some(SCRIPT.as_bytes().to_vec()) });
        revision_files.push(format!("aux/{aux_name}/revision"));
    }
    if opt {
        profile.push("opt-level = 3".to_owned());
    }
    if !profile.is_empty() {
        manifest.push_str(&format!("\n[profile.dev]\n{}\n", profile.join("\n")));
    }
    manifest.push_str("\n[workspace]\n");
    files.push(Change { path: "Cargo.toml".to_owned(), bytes: Some(manifest.into_bytes()) });
    if !pass.is_empty() {
        let quoted: Vec<String> = pass.iter().map(|it| format!("{it:?}")).collect();
        files.push(Change {
            path: ".cargo/config.toml".to_owned(),
            bytes: Some(format!("[build]\nrustflags = [{}]\n", quoted.join(", ")).into_bytes()),
        });
    }
    let mut made = Vec::new();
    for (index, label) in revisions.iter().enumerate() {
        let prefix = label.trim_end_matches(|c: char| c.is_ascii_digit());
        let (action, fails) = match prefix {
            "cpass" => (Action::Check, false),
            "cfail" => (Action::Check, true),
            "bpass" | "rpass" | "rfail" => (Action::Build, false),
            "bfail" => (Action::Build, true),
            other => return Imported::Skipped(format!("no revision prefix `{other}`")),
        };
        let mut changes = match index {
            0 => files.clone(), // clone: the package's files, the first revision's changes
            _ => Vec::new(),
        };
        for path in &revision_files {
            // clone: the path, one a change
            changes.push(Change { path: path.clone(), bytes: Some(format!("{label}\n").into_bytes()) });
        }
        made.push(Revision {
            label: label.clone(), // clone: the revision's name
            changes,
            steps: vec![Step::Edit],
            action,
            args: Vec::new(),
            env: Vec::new(),
            at_once: 1,
            expect: Expect { fails: fails.then(|| "error".to_owned()), ..Expect::default() },
            known: BTreeMap::new(),
        });
    }
    if made.len() < 2 {
        return Imported::Skipped("one revision: nothing is incremental".to_owned());
    }
    let test = Test {
        name: name.to_owned(),
        about: "rustc's tests/incremental".to_owned(),
        drivers: vec!["real".to_owned()],
        seeds: 1,
        revisions: made,
    };
    Imported::Test(Box::new(test), clean_hints(&source))
}

fn sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|it| it.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|it| it != "auxiliary") {
                sources(&path, into);
            }
        } else if path.extension().is_some_and(|it| it == "rs") {
            into.push(path);
        }
    }
}

/// A failure's cause, as a group's name: the check, and the first error line with what it
/// quotes and its numbers taken out.
fn cause(check: &str, said: &str) -> String {
    let line = said
        .lines()
        .find(|line| line.starts_with("error") && !line.starts_with("error: could not compile"))
        .or_else(|| said.lines().find(|line| line.contains("panicked")))
        .unwrap_or("no error line");
    let mut out = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '`' => {
                quoted = !quoted;
                if quoted {
                    out.push_str("`..`");
                }
            }
            _ if quoted => {}
            c if c.is_ascii_digit() => out.push('N'),
            c => out.push(c),
        }
    }
    let out: String = out.chars().take(150).collect();
    format!("{check}: {out}")
}

/// The whole corpus under `dir`, on `jobs` threads: one JSON line a test to `report`, and
/// the summary printed.
pub fn corpus(dir: &Path, report: &Path, only: Option<&str>) {
    let mut all = Vec::new();
    sources(dir, &mut all);
    let mut tests = Vec::new();
    let mut lines: Vec<serde_json::Value> = Vec::new();
    for file in &all {
        let name = file.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
        // `only`: parts of paths, with commas between.
        if only.is_some_and(|it| !it.split(',').any(|part| name.contains(part))) {
            continue;
        }
        match import(file, &name) {
            Imported::Skipped(why) if why.starts_with("no `//@ revisions:`") => {}
            Imported::Skipped(why) => lines.push(serde_json::json!({"test": name, "result": "not run", "cause": why})),
            Imported::Test(test, hints) => tests.push((test, hints, fs::metadata(file).unwrap().len())),
        }
    }
    let queue = Mutex::new(tests.into_iter());
    let done = Mutex::new(lines);
    thread::scope(|scope| {
        for _ in 0..real::jobs() {
            scope.spawn(|| {
                loop {
                    let Some((test, hints, bytes)) = queue.lock().unwrap().next() else { return };
                    let ran = catch_unwind(AssertUnwindSafe(|| real::rows(&test, Clean::WhenWrong)));
                    let line = match ran {
                        Err(payload) => {
                            let said = payload.downcast_ref::<String>().cloned().unwrap_or_else(|| "the harness panicked".to_owned());
                            serde_json::json!({"test": test.name, "result": "failed", "bytes": bytes, "cause": cause("harness", &format!("error: {}", said.lines().next().unwrap_or(""))), "said": said})
                        }
                        Ok((rows, _)) => {
                            let wrong = rows.iter().zip(&test.revisions).find(|(row, _)| !row.failed.is_empty());
                            let (mut hinted, mut remade) = (0, Vec::new());
                            for (row, revision) in rows.iter().zip(&test.revisions) {
                                for (cfg, function) in hints.iter().filter(|it| it.0 == revision.label) {
                                    hinted += 1;
                                    let again = row.seen.redone.iter().flatten().any(|it| it.rsplit("::").next() == Some(function.as_str()));
                                    if again {
                                        remade.push(format!("{cfg}: {function}"));
                                    }
                                }
                            }
                            match wrong {
                                None => serde_json::json!({"test": test.name, "result": "passed", "bytes": bytes, "clean_hints": hinted, "remade": remade}),
                                Some((row, revision)) => {
                                    let failure = &row.failed[0];
                                    // Wrong from nothing too: the front end's, not the incremental state's.
                                    let only_incremental = match (revision.expect.fails.is_some(), row.clean_ok) {
                                        (false, Some(true)) | (true, Some(false)) => true,
                                        _ => false,
                                    };
                                    let said: String = row.seen.said.lines().filter(|it| !it.trim_start().starts_with("Compiling")).take(12).collect::<Vec<_>>().join("\n");
                                    serde_json::json!({
                                        "test": test.name, "result": "failed", "bytes": bytes, "revision": row.label,
                                        "check": failure.check, "only_incremental": only_incremental,
                                        "cause": cause(failure.check, &row.seen.said), "said": said,
                                        "clean_hints": hinted, "remade": remade,
                                    })
                                }
                            }
                        }
                    };
                    println!("rustc corpus: {} {}", line["test"].as_str().unwrap_or(""), line["result"].as_str().unwrap_or(""));
                    done.lock().unwrap().push(line);
                }
            });
        }
    });
    let mut lines = done.into_inner().unwrap();
    lines.sort_by(|a, b| a["test"].as_str().cmp(&b["test"].as_str()));
    let text: String = lines.iter().map(|it| format!("{it}\n")).collect();
    fs::write(report, text).unwrap_or_else(|error| panic!("{}: {error}", report.display()));
    let count = |result: &str| lines.iter().filter(|it| it["result"] == result).count();
    println!(
        "rustc corpus: {} tests with revisions; {} passed, {} failed, {} not run; the report: {}",
        lines.len(),
        count("passed"),
        count("failed"),
        count("not run"),
        report.display()
    );
    let mut groups: BTreeMap<String, Vec<&serde_json::Value>> = BTreeMap::new();
    for line in lines.iter().filter(|it| it["result"] != "passed") {
        groups.entry(line["cause"].as_str().unwrap_or("?").to_owned()).or_default().push(line);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_key(|(_, tests)| std::cmp::Reverse(tests.len()));
    for (cause, tests) in groups {
        let smallest = tests.iter().min_by_key(|it| it["bytes"].as_u64().unwrap_or(u64::MAX)).unwrap();
        println!("  {} x {cause}; the smallest: {}", tests.len(), smallest["test"].as_str().unwrap_or(""));
    }
}
