//! gate_changes: which tier of blaze.yaml a change runs (its `changes` job). The port of
//! `.github/scripts/gate_changes.py`, deleted once both were run on the same inputs and agreed
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is what it prints when
//! not given one argument; it says what each path list is.
//!
//!     blaze run -q -p scripts --bin gate_changes -- '{"tier": "gate", "repository": "indexable-inc/blaze", "base": SHA, "head": SHA}'

/// What it prints when not given one argument: the Python's docstring, with this script's command.
const DOC: &str = r#"gate_changes.py: which tier of blaze.yaml a change runs (its `changes` job).

    blaze run -q -p scripts --bin gate_changes -- '{"tier": "gate", "repository": "indexable-inc/blaze", "base": SHA, "head": SHA}'

`tier` is the one the event asks for (`gate` for a pull request, `warm` for a push to main or a
run by hand); it is what runs when the change between `base` and `head` touches a file the
tier's jobs read (CODE in scripts/src/bin/gate_changes.rs, and WARM for `warm`), and `none`
when it touches nothing else.
Anything that cannot be told counts as code: no `base` (a run by hand, a new branch), a
comparison GitHub refuses or cuts short (300 files), or no file at all. Prints `tier=...` and
writes it to $GITHUB_OUTPUT.

An allow-list, not a deny-list: a new directory of documents, benches or harness scripts runs
no build until a tier reads it. The scripts' own checks (`harness/imports.py`, `rustcsites` and
the rest) are blaze.yaml's `scripts` job, which runs on every change.

The JSON is inline, a file's path, or `-` for stdin. Needs gh (GH_TOKEN).
"#;

use std::{io::Write, path::Path, time::Duration};

use blaze_script::{Value, pyfmt::repr, run_within};

const SCRIPT: &str = "gate_changes.py";
/// What the build of blaze and the unit tests (blaze-platform.yaml's `build` and `tests` jobs,
/// tiers gate and warm) read. A path ending in `/` is the directory and everything under it.
const CODE: &[&str] = &[
    // The workspace's crates, with their test data and crates/hir-ty-codegen/llvm.json (the LLVM
    // the blaze action fetches).
    "crates/",
    // The patched crates of [patch.crates-io], hashed into BLAZE_SOURCES (blaze-engine/build.rs).
    "vendor/",
    // ix's linter and rewriter, workspace members, and the rules the workspace check runs.
    "tools/astlog/",
    // The workspace, the lock file (the build is --locked) and cargo's config ([env]).
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/",
    // Found by blaze-fmt's and blaze-lints' search up from a directory; not known to be unread
    // by their tests.
    ".rustfmt.toml",
    ".clippy.toml",
    // The pinned nightly: compiled into blaze-cargo.
    "harness/toolchain",
    // The tests job runs it, and holds its failures to the known ones.
    "harness/unittests.py",
    "harness/failures.py",
    "harness/known-failures.toml",
    // What fetches and checks the pinned LLVM for the build (.github/actions/blaze). No Nix:
    // neither job reads the flake.
    ".github/scripts/llvm.py",
    // The profile-gate job: its script, its known list, and the flake that pins its DuckDB (its
    // fixtures are under crates/).
    "harness/profile_gate.py",
    "harness/profile-gate-known.toml",
    // The same job's smoke of the budget harness against the blaze the build made: the script,
    // the engine adapter and its layout of a daemon's home, the fixture it builds, its result.
    "harness/v2/budgets.py",
    "harness/v2/guards.py",
    "harness/v2/guards-known.toml",
    "harness/v2/engine.py",
    "harness/v2/fixture.py",
    "harness/v2/report.py",
    "harness/storelib.py",
    "flake.nix",
    "flake.lock",
    // The release of blaze that builds and tests blaze (.github/actions/blaze), and its installer.
    ".github/blaze-version",
    "site/install.sh",
    // The gate's jobs, and the actions they use.
    ".github/workflows/blaze.yaml",
    ".github/workflows/blaze-platform.yaml",
    ".github/actions/toolchain/",
    ".github/actions/blaze/",
    // The package of the repository's scripts (scripts/src/bin/<name>.rs), a workspace member the
    // build checks, among them the workspace check's fast tier (`changed`, which packages a change
    // touches); their library is crates/blaze-script, under crates/ above.
    "scripts/",
];
/// What the warm tier (a push to main) reads besides: the std cases of the smoke corpus, run
/// against the blaze that build built.
const WARM: &[&str] = &["harness/smoke.py", "harness/smoke/"];
/// GitHub's comparison lists at most this many files; a longer one is cut short.
const MOST_FILES: usize = 300;

struct Given {
    tier: String,
    repository: String,
    base: String,
    head: String,
}

fn exit_with(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}

/// The input from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong,
/// in the Python's words (this script has its own reader, not typed_input's).
fn typed(text: &str) -> Given {
    let text = if text == "-" {
        let mut all = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut all);
        all
    } else if !text.trim_start().starts_with('{') {
        std::fs::read_to_string(text)
            .unwrap_or_else(|e| exit_with(&format!("{SCRIPT}: {}", blaze_script::os_error(&e, text))))
    } else {
        text.to_owned()
    };
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| exit_with(&format!("{SCRIPT}: the input is not JSON: {e}")));
    let Value::Object(value) = value else { exit_with(&format!("{SCRIPT}: the input is a JSON object")) };
    let known = ["base", "head", "repository", "tier"];
    let mut unknown: Vec<&str> = value.keys().map(String::as_str).filter(|it| !known.contains(it)).collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        exit_with(&format!("{SCRIPT}: no field {} (fields: {})", unknown.join(", "), known.join(", ")));
    }
    let missing: Vec<&str> = ["repository", "tier"].into_iter().filter(|it| !value.contains_key(*it)).collect();
    if !missing.is_empty() {
        exit_with(&format!("{SCRIPT}: field {} is missing", missing.join(", ")));
    }
    let tier = &value["tier"];
    if !matches!(tier.as_str(), Some("gate" | "warm")) {
        exit_with(&format!("{SCRIPT}: field tier is gate or warm, not {}", repr(tier)));
    }
    for name in ["repository", "base", "head"] {
        if let Some(it) = value.get(name)
            && !it.is_string()
        {
            exit_with(&format!("{SCRIPT}: field {name} is a string, not {}", repr(it)));
        }
    }
    let field = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    Given { tier: field("tier"), repository: field("repository"), base: field("base"), head: field("head") }
}

fn is_code(path: &str, paths: &[&str]) -> bool {
    paths.iter().any(|it| if it.ends_with('/') { path.starts_with(it) } else { path == *it })
}

/// The last `n` characters of `text`, as Python's `text[-n:]`.
fn tail(text: &str, n: usize) -> &str {
    let count = text.chars().count();
    let skip = text.char_indices().nth(count.saturating_sub(n)).map_or(text.len(), |(at, _)| at);
    &text[skip..]
}

/// The files of the comparison, or None when they cannot be told (said on stderr).
fn changed(given: &Given) -> Option<Vec<String>> {
    if given.base.is_empty() || given.head.is_empty() || given.base.chars().all(|c| c == '0') {
        eprintln!("no base or head to compare: counted as code");
        return None;
    }
    let path = format!("repos/{}/compare/{}...{}", given.repository, given.base, given.head);
    let argv = ["gh", "api", path.as_str(), "--jq", "[.files[].filename]"];
    let shown = argv.join(" ");
    let here = std::env::current_dir().unwrap_or_default();
    // The Python raised when gh could not start (a traceback, exit status 1).
    let done = run_within(&argv, &here, Duration::from_secs(120)).unwrap_or_else(|e| exit_with(&format!("{SCRIPT}: {e}")));
    let Some(done) = done else {
        eprintln!("`{shown}` took over 120 s: counted as code");
        return None;
    };
    if done.status != 0 {
        let said = done.err();
        eprintln!("`{shown}` exited {}: {}: counted as code", done.status, tail(said.trim(), 1500));
        return None;
    }
    let files: Vec<String> = serde_json::from_slice::<Value>(&done.stdout)
        .ok()
        .and_then(|it| it.as_array().map(|items| items.iter().map(|f| f.as_str().unwrap_or("").to_owned()).collect()))
        .unwrap_or_else(|| exit_with(&format!("{SCRIPT}: `{shown}` printed no JSON list")));
    if files.is_empty() || files.len() >= MOST_FILES {
        eprintln!("{} files in the comparison: counted as code", files.len());
        return None;
    }
    Some(files)
}

fn decide(given: &Given) -> String {
    let mut paths: Vec<&str> = CODE.to_vec();
    if given.tier == "warm" {
        paths.extend(WARM);
    }
    let Some(files) = changed(given) else { return given.tier.clone() };
    let code: Vec<&String> = files.iter().filter(|it| is_code(it, &paths)).collect();
    for it in &files {
        eprintln!("{}  {it}", if code.contains(&it) { "code" } else { "    " });
    }
    if code.is_empty() { "none".to_owned() } else { given.tier.clone() }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [one] = args.as_slice() else { exit_with(DOC) };
    let tier = decide(&typed(one));
    println!("tier={tier}");
    if let Ok(out) = std::env::var("GITHUB_OUTPUT")
        && !out.is_empty()
    {
        let written = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(Path::new(&out))
            .and_then(|mut f| f.write_all(format!("tier={tier}\n").as_bytes()));
        if let Err(e) = written {
            exit_with(&format!("{SCRIPT}: {}", blaze_script::os_error(&e, &out)));
        }
    }
}
