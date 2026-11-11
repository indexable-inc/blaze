//! invariants: blaze's invariants, refinements and reference models as a list people and tools
//! read. The port of `harness/v2/invariants.py`, line for line: the same one JSON argument,
//! `selftest`, text printed, exit status and bytes written (docs/v2/plan/design/scripts.md,
//! "Ports and their parity"). `DOC` below is the Python's docstring, which its usage error
//! prints; it says what a statement is and what the commands do.
//!
//!     blaze run -q -p scripts --bin invariants -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin invariants -- selftest
//!
//! Where this differs from the Python, on purpose: a `git ls-files` that fails ends the script
//! with its words, where the Python raised `CalledProcessError` (a traceback, exit status 1).

/// harness/v2/invariants.py's docstring, as its usage error prints it.
const DOC: &str = r#"invariants.py: blaze's invariants, refinements and reference models as a list people and tools
read (docs/v2/plan.md, "Proof by check"). The code is their one home; this script finds them,
holds each to its proof, and writes the list, docs/v2/plan/invariants.md.

A statement is a doc comment on the item that states or checks it, opening with one of

    /// The invariant: <what always holds>          on the test whose oracle checks it
    /// The refinement: <what equals what>           on the test that runs both and compares
    /// The reference model: <what it is the simple truth of>   on the model's type or function

and running to the end of the doc comment. It is the doc comment's first line, so the words in the
middle of other prose are not a statement. `crates/harness`, a workspace of its own, is not read. An invariant or a refinement is proved only with a
control beside it: a test in the same file named `the_oracle_catches_<the planted bug>`, which
plants a bug and requires the oracle to find it (rule dst-not-review: a green test with a blind
oracle proves nothing).

    uv run --frozen python harness/v2/invariants.py '{"command": "check"}'
    uv run --frozen python harness/v2/invariants.py '{"command": "write"}'
    uv run --frozen python harness/v2/invariants.py selftest

    command   check   every invariant and refinement has a control in its file, every control
                      has a statement in its file, no two statements share an id, and
                      invariants.md is what `write` would write (CI). So removing or rewording
                      an invariant is a visible change to that file in the same pull request
              write   regenerate invariants.md
    root      the checkout (".")

An entry's id is `<crate>::<item>`, so it moves only when the item is renamed.

Each invariant and refinement has a level, the strength of the evidence for it (docs/v2/plan.md,
"The ladder"):

    tested         a property test found no counterexample
    simulated      nor did the deterministic simulation, with faults, crashes and interleavings
    model-checked  none exists up to a bound (`blaze_check::model`)
    proved         none exists: a Lean proof, named in the statement

The level is read from the file (it uses the simulation, or the model checker), or said in the
statement's opening, which wins: `/// The invariant (proved: proofs/Store/Map.lean): ...`. A proof
named must be a file in the tree.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    fail, git,
    input::{self, Fields, Input, InputError},
    pystr::{lstrip, splitlines, strip},
    read_text,
    regex::Regex,
};

const SCRIPT: &str = "invariants.py";
const LIST: &str = "docs/v2/plan/invariants.md";
const KINDS: [(&str, &str); 3] =
    [("invariant", "Invariants"), ("refinement", "Refinements"), ("model", "Reference models")];
const LEVELS: [&str; 4] = ["tested", "simulated", "model-checked", "proved"];

/// The patterns of invariants.py, compiled once; none has a lookaround or a backreference.
struct Patterns {
    open: Regex,
    doc: Regex,
    item: Regex,
    control: Regex,
    sim: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| match Regex::new(pattern) {
            Ok(it) => it,
            Err(e) => fail(SCRIPT, &format!("a pattern of its own does not compile: {e}")),
        };
        Patterns {
            open: re(r"^\s*/// The (invariant|refinement|reference model)(?: \(([a-z-]+)(?:: ([^)]+))?\))?: (.*)$"),
            doc: re(r"^\s*///(?: (.*))?$"),
            item: re(r"^\s*(?:pub(?:\([^)]*\))? )?(?:const |async |unsafe )*(?:fn|struct|enum|trait|type) (\w+)"),
            control: re(r"^\s*fn (the_oracle_catches_\w+)\("),
            sim: re(r"\bsim::|\bSim\b|hegel::sim"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: String,
    krate: String,
    item: String,
    statement: String,
    path: String,
    line: usize,
    level: String,
    proof: String,
}

impl Entry {
    fn id(&self) -> String {
        format!("{}::{}", self.krate, self.item)
    }
}

struct Found {
    entries: Vec<Entry>,
    /// By file, in the order the files were read (sorted).
    controls: BTreeMap<String, Vec<String>>,
}

struct Args {
    command: String,
    root: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args { command: f.one_of("command", &["check", "write"], None)?, root: f.str("root", Some("."))? })
    }
}

fn crate_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().position(|it| *it == "src").unwrap_or(parts.len() - 1);
    if at >= 1 { parts[at - 1] } else { parts[0] }.to_owned()
}

/// The level a file's own machinery gives its statements.
fn inferred(p: &Patterns, text: &str) -> &'static str {
    if text.contains("blaze_check::") {
        return "model-checked";
    }
    if p.sim.is_match(text) {
        return "simulated";
    }
    "tested"
}

/// The statements and the controls of one file; `Err` is the refusal, the Python's `SystemExit`.
fn scan(p: &Patterns, path: &str, text: &str) -> Result<(Vec<Entry>, Vec<String>), String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut controls: Vec<String> = Vec::new();
    let lines = splitlines(text);
    let mut i = 0;
    while i < lines.len() {
        if let Some(control) = p.control.captures(lines[i]) {
            controls.push(control[1].to_owned());
        }
        let opened = p.open.captures(lines[i]);
        let Some(opened) = opened.filter(|_| !(i > 0 && p.doc.is_match(lines[i - 1]))) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut words: Vec<&str> = vec![opened.get(4).map_or("", |m| m.as_str())];
        i += 1;
        while i < lines.len() {
            let Some(more) = p.doc.captures(lines[i]) else { break };
            words.push(more.get(1).map_or("", |m| m.as_str()));
            i += 1;
        }
        while i < lines.len() && (lstrip(lines[i]).starts_with("#[") || lstrip(lines[i]).starts_with("//")) {
            i += 1;
        }
        let item = if i < lines.len() { p.item.captures(lines[i]) } else { None };
        let Some(item) = item else {
            return Err(format!(
                "{SCRIPT}: {path}:{}: the statement is on no fn, struct, enum, trait or type",
                start + 1
            ));
        };
        let statement: Vec<&str> = words.iter().map(|w| strip(w)).filter(|w| !w.is_empty()).collect();
        let kind = &opened[1];
        let kind = kind.strip_prefix("reference ").unwrap_or(kind);
        let level = match opened.get(2) {
            Some(said) if !said.as_str().is_empty() => said.as_str(),
            _ if kind == "model" => "",
            _ => inferred(p, text),
        };
        if !level.is_empty() && !LEVELS.contains(&level) {
            return Err(format!(
                "{SCRIPT}: {path}:{}: the level `{level}` is none of {}",
                start + 1,
                LEVELS.join(", ")
            ));
        }
        entries.push(Entry {
            kind: kind.to_owned(),
            krate: crate_of(path),
            item: item[1].to_owned(),
            statement: statement.join(" "),
            path: path.to_owned(),
            line: start + 1,
            level: level.to_owned(),
            proof: opened.get(3).map_or("", |m| m.as_str()).to_owned(),
        });
    }
    Ok((entries, controls))
}

fn find(p: &Patterns, root: &Path) -> Found {
    let root_arg = root.to_string_lossy();
    let listed = match git(root, &["-C", root_arg.as_ref(), "ls-files", "crates/*.rs", "harness/*.rs"]) {
        Ok(out) => out,
        Err(e) => fail(SCRIPT, &e),
    };
    let mut listed: Vec<&str> = splitlines(&listed);
    listed.sort_unstable();
    let mut entries: Vec<Entry> = Vec::new();
    let mut controls: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in listed {
        if path.starts_with("crates/harness/") {
            continue;
        }
        let Some(text) = read_text(&root.join(path)) else { continue };
        if !text.contains("/// The ") && !text.contains("fn the_oracle_catches_") {
            continue;
        }
        let (found, its) = scan(p, path, &text).unwrap_or_else(|said| {
            eprintln!("{said}");
            std::process::exit(1)
        });
        entries.extend(found);
        if !its.is_empty() {
            controls.insert(path.to_owned(), its);
        }
    }
    Found { entries, controls }
}

fn problems(found: &Found, root: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in &found.entries {
        if e.level == "proved" && e.proof.is_empty() {
            out.push(format!(
                "{}:{}: {} says proved and names no proof: `(proved: <path to the proof>)`",
                e.path,
                e.line,
                e.id()
            ));
        }
        if root.is_some_and(|root| !e.proof.is_empty() && !root.join(&e.proof).is_file()) {
            out.push(format!("{}:{}: {}'s proof {} is not a file in the tree", e.path, e.line, e.id(), e.proof));
        }
    }
    let ids: Vec<String> = found.entries.iter().map(Entry::id).collect();
    let twice: BTreeSet<&String> = ids.iter().filter(|i| ids.iter().filter(|j| j == i).count() > 1).collect();
    out.extend(twice.iter().map(|i| format!("{i}: two statements have this id; rename one item")));
    let stated: BTreeSet<&str> =
        found.entries.iter().filter(|e| e.kind != "model").map(|e| e.path.as_str()).collect();
    for e in &found.entries {
        if e.kind != "model" && !found.controls.contains_key(&e.path) {
            out.push(format!(
                "{}:{}: {} has no control: add a test `the_oracle_catches_<bug>` that \
                 plants a bug and requires this oracle to find it",
                e.path,
                e.line,
                e.id()
            ));
        }
    }
    for (path, its) in &found.controls {
        if !stated.contains(path.as_str()) {
            out.push(format!(
                "{path}: {} controls no stated invariant: open the checked test's doc comment \
                 with `/// The invariant: ...`",
                its[0]
            ));
        }
    }
    out
}

fn listing(found: &Found) -> String {
    let count = |kind: &str| found.entries.iter().filter(|e| e.kind == kind).count();
    let by_level: Vec<String> = LEVELS
        .iter()
        .map(|lv| format!("{} {lv}", found.entries.iter().filter(|e| e.level == *lv).count()))
        .collect();
    let mut lines: Vec<String> = [
        "---",
        "status: current",
        "summary: Every invariant, refinement and reference model blaze states in its code, with the test that \
         checks it and the planted bugs that prove the check; generated by harness/v2/invariants.py.",
        "---",
        "# Invariants",
        "",
        "<!-- Generated by harness/v2/invariants.py from the code: edit the doc comment there, then run `write`. -->",
        "",
        "What blaze's code promises, stated once, beside the test that checks it. Each is a doc comment",
        "opening `/// The invariant:`, `/// The refinement:` or `/// The reference model:`; an invariant or a",
        "refinement counts only with a control in its file, a test that plants a bug its oracle must",
        "catch. A change to this file in a pull request is a promise added, reworded or dropped.",
        "",
    ]
    .iter()
    .map(|it| (*it).to_owned())
    .collect();
    lines.push(format!(
        "{} invariants, {} refinements, {} reference models.",
        count("invariant"),
        count("refinement"),
        count("model")
    ));
    lines.push(format!("By level: {}.", by_level.join(", ")));
    lines.push(String::new());
    for (kind, title) in KINDS {
        let of_kind: Vec<&Entry> = found.entries.iter().filter(|e| e.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        lines.extend([format!("## {title}"), String::new()]);
        let krates: BTreeSet<&str> = of_kind.iter().map(|e| e.krate.as_str()).collect();
        for krate in krates {
            lines.extend([format!("### {krate}"), String::new()]);
            for e in of_kind.iter().filter(|e| e.krate == krate) {
                let level = if e.level.is_empty() {
                    String::new()
                } else {
                    let proof = if e.proof.is_empty() { String::new() } else { format!(" (`{}`)", e.proof) };
                    format!(" *{}*{proof}", e.level)
                };
                lines.push(format!("- **`{}`**{level} (`{}`): {}", e.item, e.path, e.statement));
                if let Some(its) = found.controls.get(&e.path).filter(|its| kind != "model" && !its.is_empty()) {
                    let named: Vec<String> = its
                        .iter()
                        .map(|c| format!("`{}`", c.strip_prefix("the_oracle_catches_").unwrap_or(c)))
                        .collect();
                    lines.push(format!("  Controls: {}.", named.join(", ")));
                }
            }
            lines.push(String::new());
        }
    }
    format!("{}\n", lines.join("\n").trim_end_matches('\n'))
}

fn main_with(p: &Patterns, it: &Args) {
    // `Path(root).resolve()`: a root that is not there is found out when git is asked.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let found = find(p, &root);
    let text = listing(&found);
    if it.command == "write" {
        if let Err(e) = std::fs::write(root.join(LIST), &text) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        return;
    }
    let mut wrong = problems(&found, Some(&root));
    let list = root.join(LIST);
    if !list.is_file() || read_text(&list).as_deref() != Some(text.as_str()) {
        wrong.push(format!("{LIST} is stale: run {SCRIPT} with command `write` and commit it"));
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn scanned(p: &Patterns, path: &str, text: &str) -> (Vec<Entry>, Vec<String>) {
    scan(p, path, text).unwrap_or_else(|said| fail(&format!("{SCRIPT} selftest"), &format!("scan refused: {said}")))
}

fn selftest(p: &Patterns) {
    let text = "use x;\n\n\
        /// The invariant: a read hands back the bytes put,\n\
        /// whatever the faults.\n\
        #[hegel::test(test_cases = 200)]\n\
        fn reads_hand_back_what_was_put(tc: TestCase) {}\n\n\
        #[test]\nfn the_oracle_catches_a_failed_load_kept() {}\n\n\
        /// The reference model: a map from key to bytes.\n\
        pub(crate) struct Model { }\n";
    let (entries, controls) = scanned(p, "crates/blaze-cas/src/pool/tests.rs", text);
    let shape: Vec<(&str, &str, usize)> =
        entries.iter().map(|e| (e.kind.as_str(), e.item.as_str(), e.line)).collect();
    expect(
        shape == [("invariant", "reads_hand_back_what_was_put", 3), ("model", "Model", 11)],
        &format!("two statements: {entries:?}"),
    );
    expect(entries[0].statement == "a read hands back the bytes put, whatever the faults.", &entries[0].statement);
    expect(entries[0].id() == "blaze-cas::reads_hand_back_what_was_put", &entries[0].id());
    expect(controls == ["the_oracle_catches_a_failed_load_kept"], &format!("{controls:?}"));
    let in_file = |its: Vec<String>| BTreeMap::from([("crates/blaze-cas/src/pool/tests.rs".to_owned(), its)]);
    let found = Found { entries: entries.clone(), controls: in_file(controls.clone()) };
    expect(
        problems(&found, None).is_empty(),
        &format!("a stated and controlled invariant has no problems: {:?}", problems(&found, None)),
    );
    let blind = Found { entries: entries.clone(), controls: BTreeMap::new() };
    let blind_problems = problems(&blind, None);
    expect(blind_problems.iter().any(|p| p.contains("has no control")), "an invariant with no control is a problem");
    expect(blind_problems.iter().filter(|p| p.contains("has no control")).count() == 1, "a model needs no control");
    let loose = Found {
        entries: vec![],
        controls: BTreeMap::from([("crates/a/src/t.rs".to_owned(), vec!["the_oracle_catches_x".to_owned()])]),
    };
    expect(
        problems(&loose, None).iter().any(|p| p.contains("controls no stated invariant")),
        "a control with no statement is a problem",
    );
    let twice = Found { entries: vec![entries[0].clone(), entries[0].clone()], controls: in_file(controls.clone()) };
    expect(
        problems(&twice, None).iter().any(|p| p.contains("two statements have this id")),
        "a repeated id is a problem",
    );
    let page = listing(&found);
    expect(
        page.contains("1 invariants, 0 refinements, 1 reference models.")
            && page.contains("Controls: `a_failed_load_kept`."),
        &page,
    );
    expect(crate_of("crates/harness/harness-proto/src/migrate.rs") == "harness-proto", "a nested crate's name");
    let (inside, _) = scanned(p, "a.rs", "/// A type.\n/// The invariant: words in prose.\nstruct S;\n");
    expect(inside.is_empty(), &format!("words inside a doc comment are no statement: {inside:?}"));
    match scan(p, "a.rs", "/// The invariant: x\nlet y = 1;\n") {
        // The Python's `expect(False, ..)` raised the SystemExit its own `except` caught, so a
        // scan that refused nothing failed with these words (ported as it was).
        Ok(_) => expect(
            false,
            &format!("the refusal names the place: {SCRIPT} selftest: a statement on no item is refused"),
        ),
        Err(e) => expect(e.contains("a.rs:1"), &format!("the refusal names the place: {e}")),
    }
    expect(entries[0].level == "tested" && entries[1].level.is_empty(), &format!("levels: {entries:?}"));
    let (simmed, _) = scanned(p, "a.rs", "use blaze_core::sim::Sim;\n\n/// The invariant: x.\nfn t() {}\n");
    expect(simmed[0].level == "simulated", &format!("a file on the simulation is simulated: {simmed:?}"));
    let (proved, _) =
        scanned(p, "a.rs", "/// The refinement (proved: proofs/A.lean): the map is the model.\nfn t() {}\n");
    expect(
        (proved[0].level.as_str(), proved[0].proof.as_str(), proved[0].statement.as_str())
            == ("proved", "proofs/A.lean", "the map is the model."),
        &format!("{proved:?}"),
    );
    let (bare, _) = scanned(p, "a.rs", "/// The invariant (proved): x.\nfn t() {}\n");
    let bare = Found {
        entries: bare,
        controls: BTreeMap::from([("a.rs".to_owned(), vec!["the_oracle_catches_y".to_owned()])]),
    };
    expect(problems(&bare, None).iter().any(|p| p.contains("names no proof")), "proved needs its proof");
    match scan(p, "a.rs", "/// The invariant (certain): x.\nfn t() {}\n") {
        Ok(_) => expect(false, &format!("{SCRIPT} selftest: an unknown level is refused")),
        Err(e) => expect(e.contains("certain"), &e),
    }
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
            std::process::exit(1);
        }
    }
}
