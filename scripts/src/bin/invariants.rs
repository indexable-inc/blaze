//! invariants: blaze's invariants, refinements and reference models as a list people and tools
//! read. The port of `harness/v2/invariants.py`, deleted once both were run on the same tree and
//! agreed (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is what its
//! usage error prints; it says what a statement is and what the commands do.
//!
//!     blaze run -q -p scripts --bin invariants -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin invariants -- selftest
//!
//! Where this differed from the Python, on purpose: a `git ls-files` that fails ends the script
//! with its words, where the Python raised `CalledProcessError` (a traceback, exit status 1).

/// What the usage error prints: harness/v2/invariants.py's docstring, with this script's commands.
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

    blaze run -q -p scripts --bin invariants -- '{"command": "check"}'
    blaze run -q -p scripts --bin invariants -- '{"command": "write"}'
    blaze run -q -p scripts --bin invariants -- selftest

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

The claims (docs/v2/plan/design/claims.md): every promise of a doc comment and how it is held.

    an item of `# Guarantees` or `# Invariants`   evidenced: ends `Evidence: [`a_test`]` or
                                                  `Evidence: Lean `Name``; unverified: ends
                                                  `Unverified: why`; none: neither
    `#[blaze::nopanic]` (`noalloc`, `nonblocking`, `pure`)   checked; `trusted = "why"`: unverified
    a `# Safety` section                          obligation (the caller's)
    a `.spec.proof` theorem of lean/theorems.json  a contract, proved

A claim's id is `<crate>::<item>#<n>`. Its level: an evidence pointer to a Lean theorem that
lean/theorems.json names is proved; one to a test takes the test's file's level, as a statement
does. invariants.md lists every claim but a checked attribute and an obligation, which its totals
by crate count; `claims` prints all of them as JSON.

    command   claims  print every claim as JSON, one object per claim
    base      the commit whose invariants.md `check` holds the claims to ("HEAD"): no crate's
              count of unverified and unheld claims grows, and no claim's level goes down;
              every evidence pointer names a test of its crate (by its last segment) or a
              theorem of lean/theorems.json
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
    effect: Regex,
    trusted: Regex,
    test_attr: Regex,
    function: Regex,
    pointer: Regex,
    row: Regex,
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
            // `#[bstd::test]` (in bstd itself, `#[crate::test]`) runs its test on the simulation
            // without the file naming it (docs/v2/plan/design/bstd-sim-test.md); a blaze-sim
            // simulation holds its answers in a `Flight` (docs/v2/plan/design/sim.md). The engine
            // alone (`blaze_sim::check`) marks a property test, which is tested, not simulated.
            sim: re(r"\bsim::|\bSim\b|\bFlight\b|#\[(?:bstd|crate)::test\b"),
            effect: re(r"^\s*#\[blaze::(nopanic|noalloc|nonblocking|pure)(?:\((.*)\))?\]"),
            trusted: re(r#"trusted\s*=\s*"([^"]*)""#),
            test_attr: re(r"^\s*#\[(?:test|bstd::test|crate::test|tokio::test)\b"),
            function: re(r"^\s*(?:pub(?:\([^)]*\))? )?(?:const |async |unsafe )*fn (\w+)"),
            pointer: re(r"\[`([^`]+)`\]|Lean `([^`]+)`"),
            row: re(r"^- \*\*`([^`]+)`\*\* \*([a-z-]+)\*"),
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
    /// The claims of the doc comments and attributes, in the order the files were read.
    claims: Vec<Claim>,
    /// The tests by crate and name, each with its file's level.
    tests: BTreeMap<(String, String), Vec<String>>,
}

/// The rungs a claim's level is on, weakest first: `check` refuses a claim moving down them.
const RUNGS: [&str; 8] = ["none", "unverified", "obligation", "checked", "tested", "simulated", "model-checked", "proved"];

/// A promise of a doc comment or an attribute, and how it is held.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    krate: String,
    item: String,
    /// Its place among its item's claims, from 1.
    n: usize,
    /// `evidenced`, `unverified`, `none`, `checked`, `trusted`, `obligation` or `proved`.
    held: String,
    /// A rung of [`RUNGS`]; an evidenced claim's is set from its pointers by [`leveled`].
    level: String,
    text: String,
    /// As written: `` `tests::sorted` `` or `Lean Name`; a trusted attribute's reason.
    evidence: Vec<String>,
    path: String,
    line: usize,
}

impl Claim {
    fn id(&self) -> String {
        format!("{}::{}#{}", self.krate, self.item, self.n)
    }

    /// Whether the claim is held by nothing checked: the count `check` holds per crate.
    fn open(&self) -> bool {
        matches!(self.level.as_str(), "none" | "unverified")
    }
}

struct Args {
    command: String,
    root: String,
    base: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "claims"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str("base", Some("HEAD"))?,
        })
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

/// The module path of a crate's file: `src/sim/seeds.rs` is `sim::seeds`, `src/lib.rs` none.
fn module_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().position(|it| *it == "src").map_or(0, |it| it + 1);
    let mut module: Vec<&str> = parts[at.min(parts.len())..].iter().map(|it| it.strip_suffix(".rs").unwrap_or(it)).collect();
    if module.last().is_some_and(|it| matches!(*it, "lib" | "main" | "mod")) {
        module.pop();
    }
    module.join("::")
}

/// How the words of a section's item say it is held: its claim, `evidenced`, `unverified` or
/// `none`, and its pointers or its reason.
fn held_by(p: &Patterns, words: &str) -> (String, &'static str, Vec<String>) {
    if let Some(at) = words.find("Evidence:") {
        let pointers: Vec<String> = p
            .pointer
            .captures_iter(&words[at..])
            .filter_map(|c| match (c.get(1), c.get(2)) {
                (Some(rust), _) => Some(format!("`{}`", rust.as_str())),
                (_, Some(lean)) => Some(format!("Lean {}", lean.as_str())),
                _ => None,
            })
            .collect();
        let held = if pointers.is_empty() { "none" } else { "evidenced" };
        return (strip(&words[..at]).to_owned(), held, pointers);
    }
    if let Some(at) = words.find("Unverified:") {
        let why = strip(words[at + "Unverified:".len()..].trim_end_matches('.'));
        let (held, reason) = if why.is_empty() { ("none", vec![]) } else { ("unverified", vec![why.to_owned()]) };
        return (strip(&words[..at]).to_owned(), held, reason);
    }
    (strip(words).to_owned(), "none", vec![])
}

/// The claims of one file and the names of its tests (a function with a test attribute, or any
/// function of a test file: an oracle, a simulation's helper).
fn scan_claims(p: &Patterns, path: &str, text: &str) -> (Vec<Claim>, Vec<String>) {
    let lines = splitlines(text);
    let test_file = path.ends_with("tests.rs") || path.contains("/tests/");
    let krate = crate_of(path);
    let module = module_of(path);
    let mut claims: Vec<Claim> = Vec::new();
    let mut tests: Vec<String> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(item) = p.item.captures(line) else { continue };
        let name = item[1].to_owned();
        // The attribute and comment lines above the item, then its doc comment.
        let mut top = at;
        let mut attrs: Vec<&str> = Vec::new();
        while top > 0 {
            let above = lstrip(lines[top - 1]);
            if !(above.starts_with("#[") || (above.starts_with("//") && !above.starts_with("///"))) {
                break;
            }
            attrs.push(above);
            top -= 1;
        }
        let mut doc: Vec<(usize, &str)> = Vec::new();
        while top > 0 {
            let Some(said) = p.doc.captures(lines[top - 1]) else { break };
            doc.push((top - 1, said.get(1).map_or("", |m| m.as_str())));
            top -= 1;
        }
        doc.reverse();
        if p.function.is_match(line) && (test_file || attrs.iter().any(|it| p.test_attr.is_match(it))) {
            tests.push(name.clone());
        }
        let item = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
        let mut claim = |held: &str, level: &str, text: String, evidence: Vec<String>, line: usize| {
            let n = seen.entry(item.clone()).or_insert(0);
            *n += 1;
            claims.push(Claim {
                krate: krate.clone(),
                item: item.clone(),
                n: *n,
                held: held.to_owned(),
                level: level.to_owned(),
                text,
                evidence,
                path: path.to_owned(),
                line: line + 1,
            });
        };
        for attr in attrs.iter().rev() {
            let Some(effect) = p.effect.captures(attr) else { continue };
            let said = format!("`{name}` is {}", &effect[1]);
            match p.trusted.captures(effect.get(2).map_or("", |m| m.as_str())) {
                Some(why) => claim("trusted", "unverified", said, vec![why[1].to_owned()], at),
                None => claim("checked", "checked", said, vec![], at),
            }
        }
        let mut section = "";
        let mut in_code = false;
        let mut fresh = true;
        let mut items: Vec<(usize, String)> = Vec::new();
        for &(ln, said) in &doc {
            let words = strip(said);
            if words.starts_with("```") {
                in_code = !in_code;
                continue;
            }
            if in_code {
                continue;
            }
            if let Some(heading) = words.strip_prefix('#') {
                section = strip(heading.trim_start_matches('#'));
                fresh = true;
                if section == "Safety" {
                    claim("obligation", "obligation", format!("the caller of `{name}` upholds its `# Safety`"), vec![], ln);
                }
                continue;
            }
            if section != "Guarantees" && section != "Invariants" {
                continue;
            }
            if words.is_empty() {
                fresh = true;
                continue;
            }
            match words.strip_prefix("- ").or_else(|| words.strip_prefix("* ")) {
                Some(bullet) => items.push((ln, bullet.to_owned())),
                None if fresh || items.is_empty() => items.push((ln, words.to_owned())),
                None => {
                    if let Some(last) = items.last_mut() {
                        last.1.push(' ');
                        last.1.push_str(words);
                    }
                }
            }
            fresh = false;
        }
        for (ln, words) in items {
            let (said, held, evidence) = held_by(p, &words);
            // An evidenced claim's level comes from what its pointers name ([`leveled`]).
            let level = if held == "unverified" { "unverified" } else { "none" };
            claim(held, level, said, evidence, ln);
        }
    }
    (claims, tests)
}

/// The theorems of lean/theorems.json by name, and each contract proof (`<path>.spec.proof`) as
/// a claim, proved.
fn theorems(root: &Path) -> (BTreeSet<String>, Vec<Claim>) {
    const FILE: &str = "lean/theorems.json";
    let Some(text) = read_text(&root.join(FILE)) else { fail(SCRIPT, &format!("{FILE} is not readable")) };
    let value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| fail(SCRIPT, &format!("{FILE} is not JSON: {e}")));
    let Some(listed) = value.get("theorems").and_then(serde_json::Value::as_array) else {
        fail(SCRIPT, &format!("{FILE} has no `theorems` list"))
    };
    let mut names = BTreeSet::new();
    let mut contracts = Vec::new();
    for theorem in listed {
        let Some(name) = theorem.get("name").and_then(serde_json::Value::as_str) else {
            fail(SCRIPT, &format!("{FILE}: a theorem has no `name`"))
        };
        names.insert(name.to_owned());
        if let Some(path) = name.strip_suffix(".spec.proof") {
            let mut segments = path.split('.');
            let krate = segments.next().unwrap_or_default().replace('_', "-");
            let item: Vec<&str> = segments.collect();
            let says = theorem.get("says").and_then(serde_json::Value::as_str).unwrap_or_default();
            contracts.push(Claim {
                krate,
                item: item.join("::"),
                n: 1,
                held: "proved".to_owned(),
                level: "proved".to_owned(),
                text: says.to_owned(),
                evidence: vec![format!("Lean {name}")],
                path: FILE.to_owned(),
                line: 0,
            });
        }
    }
    (names, contracts)
}

fn rung(level: &str) -> usize {
    RUNGS.iter().position(|it| *it == level).unwrap_or(0)
}

/// What a pointer names: `Some(level)` of a theorem or a test, `None` when it names nothing.
fn pointed(found: &Found, theorems: &BTreeSet<String>, krate: &str, pointer: &str) -> Option<String> {
    if let Some(name) = pointer.strip_prefix("Lean ") {
        return theorems.contains(name).then(|| "proved".to_owned());
    }
    let path = pointer.trim_matches('`');
    let name = path.rsplit("::").next().unwrap_or(path).trim_end_matches("()");
    let levels = found.tests.get(&(krate.to_owned(), name.to_owned()))?;
    // Two tests of one name in a crate: the weaker is what the pointer is sure of.
    levels.iter().min_by_key(|it| rung(it)).cloned()
}

/// Each evidenced claim's level: the strongest of what its pointers name.
fn leveled(found: &mut Found, theorems: &BTreeSet<String>) {
    let mut levels: Vec<Option<String>> = Vec::new();
    for claim in &found.claims {
        let best = (claim.held == "evidenced")
            .then(|| {
                claim.evidence.iter().filter_map(|it| pointed(found, theorems, &claim.krate, it)).max_by_key(|it| rung(it))
            })
            .flatten();
        levels.push(best);
    }
    for (claim, level) in found.claims.iter_mut().zip(levels) {
        if let Some(level) = level {
            claim.level = level;
        }
    }
}

/// Whether a claim has a row in invariants.md: all but a checked attribute and an obligation,
/// which the totals count.
fn has_row(claim: &Claim) -> bool {
    !matches!(claim.held.as_str(), "checked" | "obligation")
}

/// The claims' part of invariants.md.
fn claims_listing(found: &Found) -> String {
    let mut lines: Vec<String> = vec![
        "## Claims".to_owned(),
        String::new(),
        "Every promise of a doc comment or an effect attribute and how it is held".to_owned(),
        "(docs/v2/plan/design/claims.md): an item of `# Guarantees` or `# Invariants` ending in".to_owned(),
        "`Evidence:` or `Unverified:`, an effect attribute, a `# Safety` obligation, a proved contract.".to_owned(),
        "A checked attribute and an obligation are counted, not listed.".to_owned(),
        String::new(),
    ];
    let by_rung: Vec<String> = RUNGS
        .iter()
        .map(|lv| format!("{} {lv}", found.claims.iter().filter(|c| c.level == *lv).count()))
        .collect();
    lines.push(format!("{} claims. By level: {}.", found.claims.len(), by_rung.join(", ")));
    lines.extend([String::new(), "### Totals".to_owned(), String::new()]);
    lines.push(format!("| crate | {} |", RUNGS.join(" | ")));
    lines.push(format!("| --- |{}", " ---: |".repeat(RUNGS.len())));
    let krates: BTreeSet<&str> = found.claims.iter().map(|c| c.krate.as_str()).collect();
    for krate in krates {
        let counts: Vec<String> = RUNGS
            .iter()
            .map(|lv| found.claims.iter().filter(|c| c.krate == krate && c.level == *lv).count().to_string())
            .collect();
        lines.push(format!("| {krate} | {} |", counts.join(" | ")));
    }
    lines.extend([String::new(), "### Listed".to_owned(), String::new()]);
    let mut listed: Vec<&Claim> = found.claims.iter().filter(|c| has_row(c)).collect();
    listed.sort_by_key(|c| c.id());
    for c in listed {
        let place = if c.line == 0 { c.path.clone() } else { format!("{}:{}", c.path, c.line) };
        let evidence = match c.held.as_str() {
            "evidenced" | "proved" => format!(" Evidence: {}.", c.evidence.join(", ")),
            "unverified" | "trusted" => format!(" Unverified: {}.", c.evidence.join(", ")),
            _ => String::new(),
        };
        lines.push(format!("- **`{}`** *{}* {} (`{place}`): {}{evidence}", c.id(), c.level, c.held, c.text));
    }
    format!("{}\n", lines.join("\n").trim_end_matches('\n'))
}

/// invariants.md: the statements, then the claims.
fn page(found: &Found) -> String {
    format!("{}\n{}", listing(found), claims_listing(found))
}

/// What a ledger at `base` held: each listed claim's level, and each crate's open count.
fn base_ledger(p: &Patterns, text: &str) -> Option<(BTreeMap<String, String>, BTreeMap<String, usize>)> {
    let at = text.find("\n## Claims\n")?;
    let mut levels = BTreeMap::new();
    let mut open = BTreeMap::new();
    for line in splitlines(&text[at..]) {
        if let Some(row) = p.row.captures(line) {
            levels.insert(row[1].to_owned(), row[2].to_owned());
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(strip).collect();
        // `| crate | none | unverified | ...`: the empty cells either side of the bars.
        if cells.len() == RUNGS.len() + 3 && cells[1] != "crate" && !cells[1].starts_with("---") {
            let count = |at: usize| cells[at].parse::<usize>().unwrap_or(0);
            open.insert(cells[1].to_owned(), count(2) + count(3));
        }
    }
    Some((levels, open))
}

/// What `check` refuses of the claims: a pointer that names nothing, and against `base`'s
/// ledger, a crate whose open claims grew or a claim whose level went down.
fn claim_problems(found: &Found, theorems: &BTreeSet<String>, base: Option<(BTreeMap<String, String>, BTreeMap<String, usize>)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in found.claims.iter().filter(|c| c.held == "evidenced") {
        for pointer in &c.evidence {
            if pointed(found, theorems, &c.krate, pointer).is_none() {
                out.push(format!(
                    "{}:{}: {}'s evidence {pointer} names no test of {} and no theorem of lean/theorems.json",
                    c.path,
                    c.line,
                    c.id(),
                    c.krate
                ));
            }
        }
    }
    let Some((levels, open)) = base else { return out };
    for c in found.claims.iter().filter(|c| has_row(c)) {
        if let Some(was) = levels.get(&c.id())
            && rung(&c.level) < rung(was)
        {
            out.push(format!("{}:{}: {}'s level went down, from {was} to {}", c.path, c.line, c.id(), c.level));
        }
    }
    let mut now: BTreeMap<&str, usize> = BTreeMap::new();
    for c in found.claims.iter().filter(|c| c.open()) {
        *now.entry(c.krate.as_str()).or_insert(0) += 1;
    }
    for (krate, count) in now {
        let was = open.get(krate).copied().unwrap_or(0);
        if count > was {
            out.push(format!(
                "{krate}: {count} claims held by nothing or by a reason, {was} at the base: evidence the new ones, or say why"
            ));
        }
    }
    out
}

/// Every claim as JSON, one object per claim.
fn claims_json(found: &Found) -> String {
    let all: Vec<serde_json::Value> = found
        .claims
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id(), "crate": c.krate, "item": c.item, "held": c.held, "level": c.level,
                "claim": c.text, "evidence": c.evidence, "path": c.path, "line": c.line,
            })
        })
        .collect();
    serde_json::to_string_pretty(&all).unwrap_or_else(|e| fail(SCRIPT, &format!("the claims are not JSON: {e}")))
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
    let mut claims: Vec<Claim> = Vec::new();
    let mut tests: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for path in listed {
        if path.starts_with("crates/harness/") {
            continue;
        }
        let Some(text) = read_text(&root.join(path)) else { continue };
        let (claimed, its_tests) = scan_claims(p, path, &text);
        claims.extend(claimed);
        if !its_tests.is_empty() {
            let level = inferred(p, &text);
            for name in its_tests {
                tests.entry((crate_of(path), name)).or_default().push(level.to_owned());
            }
        }
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
    Found { entries, controls, claims, tests }
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
         checks it and the planted bugs that prove the check; generated by scripts/src/bin/invariants.rs.",
        "---",
        "# Invariants",
        "",
        "<!-- Generated by scripts/src/bin/invariants.rs from the code: edit the doc comment there, then run `write`. -->",
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
    let mut found = find(p, &root);
    let (names, contracts) = theorems(&root);
    found.claims.extend(contracts);
    leveled(&mut found, &names);
    if it.command == "claims" {
        println!("{}", claims_json(&found));
        return;
    }
    let text = page(&found);
    if it.command == "write" {
        if let Err(e) = std::fs::write(root.join(LIST), &text) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        return;
    }
    let mut wrong = problems(&found, Some(&root));
    let root_arg = root.to_string_lossy();
    let shown = format!("{}:{LIST}", it.base);
    let base = match git(&root, &["-C", root_arg.as_ref(), "show", &shown]) {
        Ok(text) => text,
        Err(e) => fail(SCRIPT, &format!("the base ledger {shown} is not readable: {e}")),
    };
    let held = base_ledger(p, &base);
    if held.is_none() {
        eprintln!("{SCRIPT}: {shown} has no Claims section: this is the first ledger, nothing to hold the claims to");
    }
    wrong.extend(claim_problems(&found, &names, held));
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
        #[test]\n\
        fn reads_hand_back_what_was_put() { blaze_sim::check(Config::cases(200), |tape| {}); }\n\n\
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
    let found =
        Found { entries: entries.clone(), controls: in_file(controls.clone()), claims: vec![], tests: BTreeMap::new() };
    expect(
        problems(&found, None).is_empty(),
        &format!("a stated and controlled invariant has no problems: {:?}", problems(&found, None)),
    );
    let blind = Found { entries: entries.clone(), controls: BTreeMap::new(), claims: vec![], tests: BTreeMap::new() };
    let blind_problems = problems(&blind, None);
    expect(blind_problems.iter().any(|p| p.contains("has no control")), "an invariant with no control is a problem");
    expect(blind_problems.iter().filter(|p| p.contains("has no control")).count() == 1, "a model needs no control");
    let loose = Found {
        entries: vec![],
        controls: BTreeMap::from([("crates/a/src/t.rs".to_owned(), vec!["the_oracle_catches_x".to_owned()])]),
        claims: vec![],
        tests: BTreeMap::new(),
    };
    expect(
        problems(&loose, None).iter().any(|p| p.contains("controls no stated invariant")),
        "a control with no statement is a problem",
    );
    let twice = Found {
        entries: vec![entries[0].clone(), entries[0].clone()],
        controls: in_file(controls.clone()),
        claims: vec![],
        tests: BTreeMap::new(),
    };
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
        Ok(_) => expect(false, "a statement on no item is refused"),
        Err(e) => expect(e.contains("a.rs:1"), &format!("the refusal names the place: {e}")),
    }
    expect(
        entries[0].level == "tested" && entries[1].level.is_empty(),
        &format!("a property test on the tape is tested, a model has no level: {entries:?}"),
    );
    let (plain, _) = scanned(p, "a.rs", "/// The invariant: x.\n#[test]\nfn t() { assert_eq!(f(1), 2); }\n");
    expect(plain[0].level == "tested", &format!("a plain test is tested: {plain:?}"));
    let (flight, _) = scanned(
        p,
        "a.rs",
        "use blaze_sim::{Config, Flight};\n\n/// The invariant: x.\n#[test]\nfn t() { blaze_sim::check(Config::cases(64), |tape| { let mut flight = Flight::new(); }); }\n",
    );
    expect(flight[0].level == "simulated", &format!("a test that holds a `Flight` is simulated: {flight:?}"));
    let (simmed, _) = scanned(p, "a.rs", "use blaze_core::sim::Sim;\n\n/// The invariant: x.\nfn t() {}\n");
    expect(simmed[0].level == "simulated", &format!("a file on the simulation is simulated: {simmed:?}"));
    let (seeded, _) = scanned(p, "a.rs", "/// The invariant: x.\n#[bstd::test(without(Crashes))]\nfn t(cx: &Cx, io: Io) {}\n");
    expect(seeded[0].level == "simulated", &format!("a `#[bstd::test]` is simulated: {seeded:?}"));
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
        claims: vec![],
        tests: BTreeMap::new(),
    };
    expect(problems(&bare, None).iter().any(|p| p.contains("names no proof")), "proved needs its proof");
    match scan(p, "a.rs", "/// The invariant (certain): x.\nfn t() {}\n") {
        Ok(_) => expect(false, "an unknown level is refused"),
        Err(e) => expect(e.contains("certain"), &e),
    }
    claims_selftest(p);
    println!("{SCRIPT} selftest: ok");
}

/// The claims: each way one is held, its level from what its pointers name, and what `check`
/// refuses against a base ledger.
fn claims_selftest(p: &Patterns) {
    let lib = "/// Mixes the pair.\n\
        ///\n\
        /// # Guarantees\n\
        ///\n\
        /// - Symmetric. Evidence: [`tests::mixes_either_way`]\n\
        /// - Deterministic. Evidence: Lean `Blaze.Mix.det`\n\
        /// - One instruction. Unverified: nothing counts them.\n\
        /// - Total.\n\
        /// - Cheap. Evidence: [`tests::gone`]\n\
        #[blaze::nopanic]\n\
        #[blaze::noalloc(trusted = \"the arena is warm\")]\n\
        pub fn mix(a: u32, b: u32) -> u32 { a ^ b }\n\
        \n\
        /// # Safety\n\
        ///\n\
        /// `p` is live.\n\
        pub unsafe fn read(p: *const u32) -> u32 { *p }\n";
    let (claims, none) = scan_claims(p, "crates/a/src/mix.rs", lib);
    expect(none.is_empty(), &format!("no tests in mix.rs: {none:?}"));
    let shape: Vec<(String, &str, &str)> =
        claims.iter().map(|c| (c.id(), c.held.as_str(), c.level.as_str())).collect();
    let want = [
        ("a::mix::mix#1", "checked", "checked"),
        ("a::mix::mix#2", "trusted", "unverified"),
        ("a::mix::mix#3", "evidenced", "none"),
        ("a::mix::mix#4", "evidenced", "none"),
        ("a::mix::mix#5", "unverified", "unverified"),
        ("a::mix::mix#6", "none", "none"),
        ("a::mix::mix#7", "evidenced", "none"),
        ("a::mix::read#1", "obligation", "obligation"),
    ];
    expect(
        shape.iter().map(|(id, held, level)| (id.as_str(), *held, *level)).eq(want.iter().copied()),
        &format!("each way a claim is held: {shape:?}"),
    );
    expect(claims[2].text == "Symmetric." && claims[2].evidence == ["`tests::mixes_either_way`"], &format!("{:?}", claims[2]));
    expect(claims[1].evidence == ["the arena is warm"], &format!("a trusted attribute's reason: {:?}", claims[1]));
    let (_, named) = scan_claims(p, "crates/a/src/x.rs", "#[test]\nfn t() {}\nfn helper() {}\n");
    expect(named == ["t"], &format!("a test by its attribute: {named:?}"));
    let (_, oracles) = scan_claims(p, "crates/a/src/tests.rs", "fn mixes_either_way() {}\n");
    expect(oracles == ["mixes_either_way"], &format!("any function of a test file: {oracles:?}"));
    expect(module_of("crates/a/src/lib.rs").is_empty() && module_of("crates/a/src/sim/mod.rs") == "sim", "module paths");

    let theorems = BTreeSet::from(["Blaze.Mix.det".to_owned()]);
    let tests = BTreeMap::from([(("a".to_owned(), "mixes_either_way".to_owned()), vec!["simulated".to_owned()])]);
    let mut found = Found { entries: vec![], controls: BTreeMap::new(), claims, tests };
    leveled(&mut found, &theorems);
    let levels: Vec<&str> = found.claims.iter().map(|c| c.level.as_str()).collect();
    expect(levels[2] == "simulated" && levels[3] == "proved" && levels[6] == "none", &format!("levels from pointers: {levels:?}"));
    let alone = claim_problems(&found, &theorems, None);
    expect(alone.len() == 1 && alone[0].contains("`tests::gone`"), &format!("a pointer that names nothing: {alone:?}"));
    let ledger = format!("# Invariants\n\n{}", claims_listing(&found));
    expect(ledger.contains("| a | 2 | 2 | 1 | 1 | 0 | 1 | 0 | 1 |"), &ledger);
    let held = |text: &str| base_ledger(p, text);
    expect(held("# Invariants\n").is_none(), "a ledger with no Claims section holds nothing");
    let same = claim_problems(&found, &theorems, held(&ledger));
    expect(same.len() == 1, &format!("the same claims hold against their own ledger: {same:?}"));
    let mut down = found.claims.clone();
    down[3].level = "tested".to_owned();
    let lowered = Found { entries: vec![], controls: BTreeMap::new(), claims: down, tests: BTreeMap::new() };
    expect(
        claim_problems(&lowered, &theorems, held(&ledger)).iter().any(|it| it.contains("a::mix::mix#4's level went down")),
        "a level going down is refused",
    );
    let mut more = found.claims.clone();
    let mut extra = more[5].clone();
    extra.n = 8;
    more.push(extra);
    let grown = Found { entries: vec![], controls: BTreeMap::new(), claims: more, tests: BTreeMap::new() };
    expect(
        claim_problems(&grown, &theorems, held(&ledger)).iter().any(|it| it.contains("a: 5 claims held by nothing")),
        "a new claim held by nothing is refused",
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern compiles with the regex features the lock builds (a `(?i)` needs
    /// `unicode-case`), so a missing feature fails `blaze test`, not the script's first run.
    #[test]
    fn the_patterns_compile() {
        let _ = Patterns::new();
    }
}
