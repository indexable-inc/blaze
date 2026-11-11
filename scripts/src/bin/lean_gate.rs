//! lean_gate: blaze's Lean proofs, held by the gate. The port of `harness/v2/lean_gate.py`, line
//! for line: the same one JSON argument, `selftest`, text printed, exit status and bytes written
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"); it runs the same `lake` commands.
//! `DOC` below is the Python's docstring, which its usage error prints.
//!
//!     blaze run -q -p scripts --bin lean_gate -- '{"command": "sources"}'
//!     blaze run -q -p scripts --bin lean_gate -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin lean_gate -- selftest
//!
//! `docs/v2/plan/proofs.md` keeps naming `harness/v2/lean_gate.py` until the switch, so both
//! write the same bytes. A refusal from finding `lake` keeps the prefix `mir_semantics.py:`, as
//! the Python's `lake_of` (imported from mir_semantics.py) printed it.

/// harness/v2/lean_gate.py's docstring, as its usage error prints it.
const DOC: &str = r#"lean_gate.py: blaze's Lean proofs, held by the gate (rule verified-compiler; ledger V5 to V10;
docs/v2/plan/design/formal-verification.md). A proof nothing rebuilds is a claim: this builds both
Lake packages with the pinned Lean, asks Lean which axioms each named theorem rests on, and holds
the sources and the list people read to what is checked in.

    uv run --frozen python harness/v2/lean_gate.py '{"command": "check"}'
    uv run --frozen python harness/v2/lean_gate.py '{"command": "sources"}'
    uv run --frozen python harness/v2/lean_gate.py '{"command": "write"}'
    uv run --frozen python harness/v2/lean_gate.py selftest

The input is one JSON object:

    command   sources  what needs no Lean (the `scripts` job, seconds): lean/theorems.json is well
                       formed, every theorem it names is stated in the module it names, each
                       ledger row it names is a row, no Lean source says `sorry`, `admit`,
                       `native_decide`, `decide +native`, `axiom`, `partial def` or
                       `debug.skipKernelTC` outside comments unless lean/theorems.json's `allow`
                       lists that file and word, the two packages pin one toolchain,
                       lean/analysis/lake-manifest.json is checked in and pins Mathlib at
                       lakefile.toml's `rev`, and docs/v2/plan/proofs.md is what `write` writes
              check    `sources`, then with the pinned `lake`: `lake build` in lean/; in
                       lean/analysis `lake exe cache get` (Mathlib's prebuilt files: Mathlib is
                       never compiled) and `lake build`; neither log may say `declaration uses
                       'sorry'`; then one generated Lean file per package runs `#print axioms` on
                       each theorem of lean/theorems.json, and each must exist and rest on nothing
                       but the axioms the file allows (`propext`, `Classical.choice`,
                       `Quot.sound`). Prints the table
              write    regenerate docs/v2/plan/proofs.md
    root      the checkout (".")
    lake      the pinned toolchain's `lake`, an absolute path; empty for the one elan keeps for
              lean/lean-toolchain's pin (`~/.elan/toolchains/<pin>/bin/lake`). Never PATH
    packages  which packages `check` builds: `core` (lean/) and `analysis` (lean/analysis); both
    out       where the verdict goes for the ledger (scripts/src/bin/ledger.rs), when given

lean/theorems.json is the one list of what blaze claims is proved:

    axioms    the axioms a theorem may rest on
    allow     `{path, word, why}`: a source that may say a refused word
    theorems  `{name, package, module, says, row}`: the theorem's full name, its package, the
              module that states it, what it says in words (one line), and its ledger row

`lean/analysis/lake-manifest.json` is checked in, so Mathlib's own dependencies are pinned too
(lakefile.toml pins Mathlib alone). It is written by `lake update` in lean/analysis with the pinned
toolchain, and only then: the gate never runs `lake update`, so a build never resolves anything.
"#;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Instant,
};

use blaze_script::{
    TempDir, fail,
    input::{self, Fields, Input, InputError},
    pyfmt::{py_str, repr_str, repr_strs},
    pyjson::dumps,
    pystr::{is_space, is_word, rstrip, splitlines, strip},
    read_text,
    regex::Regex,
    run, toml, universal_newlines,
};
use serde_json::{Value, json};

const SCRIPT: &str = "lean_gate.py";
const LIST: &str = "lean/theorems.json";
const PAGE: &str = "docs/v2/plan/proofs.md";
const LEDGER: &str = "docs/v2/plan/objectives.json";
const MANIFEST: &str = "lean/analysis/lake-manifest.json";
/// A package's name, its directory, and the directories its modules are found under, in order.
const PACKAGES: [(&str, &str, &[&str]); 2] =
    [("core", "lean", &["lean", "crates/blaze-graph/lean"]), ("analysis", "lean/analysis", &["lean/analysis"])];
const SORRY: &str = "declaration uses 'sorry'";

fn package_dir(package: &str) -> &'static str {
    PACKAGES.iter().find(|(name, _, _)| *name == package).map_or("", |(_, dir, _)| dir)
}

/// How a refused word is found: `sorry` and `admit` need a lookbehind and a lookahead, which the
/// regex crate has not, so they are matched by hand.
enum Find {
    /// Python's `(?<![\w.'])<word>(?![\w'])`.
    Bare(&'static str),
    Re(Regex),
}

impl Find {
    /// Where each match starts, as `finditer` finds them.
    fn starts(&self, code: &str) -> Vec<usize> {
        match self {
            Find::Bare(word) => code
                .match_indices(word)
                .filter(|(at, _)| {
                    let before = code[..*at].chars().next_back();
                    let after = code[at + word.len()..].chars().next();
                    !before.is_some_and(|c| is_word(c) || c == '.' || c == '\'')
                        && !after.is_some_and(|c| is_word(c) || c == '\'')
                })
                .map(|(at, _)| at)
                .collect(),
            Find::Re(re) => re.find_iter(code).map(|m| m.start()).collect(),
        }
    }

    fn found_in(&self, code: &str) -> bool {
        !self.starts(code).is_empty()
    }
}

struct Patterns {
    /// What no proof may say, outside comments: each defeats the kernel's check or adds to what
    /// is trusted.
    refused: Vec<(&'static str, Find)>,
    axioms: Regex,
    no_axioms: Regex,
    name: Regex,
    statement_end: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        Patterns {
            refused: vec![
                ("sorry", Find::Bare("sorry")),
                ("admit", Find::Bare("admit")),
                ("native_decide", Find::Re(re(r"native_decide"))),
                ("decide +native", Find::Re(re(r"\+native\b"))),
                ("axiom", Find::Re(re(r"(?m)^\s*(?:(?:private|protected|noncomputable|unsafe)\s+)*axiom\s"))),
                ("partial def", Find::Re(re(r"\bpartial\s+def\b"))),
                ("debug.skipKernelTC", Find::Re(re(r"skipKernelTC"))),
            ],
            axioms: re(r"'([^'\s]+)' depends on axioms: \[([^\]]*)\]"),
            no_axioms: re(r"'([^'\s]+)' does not depend on any axioms"),
            // Python's `NAME.match`: its `$` also matches before a last newline.
            name: re(r"\A[A-Za-z_][\w']*(?:\.[A-Za-z_][\w']*)*\n?\z"),
            statement_end: re(r"(?m)\s*:=|\s+where\s*$"),
        }
    }

    fn find(&self, word: &str) -> Option<&Find> {
        self.refused.iter().find(|(it, _)| *it == word).map(|(_, find)| find)
    }
}

#[derive(Debug, Clone)]
struct Theorem {
    name: String,
    package: String,
    module: String,
    says: String,
    row: String,
}

impl Input for Theorem {
    const FIELDS: &'static [&'static str] = &["name", "package", "module", "says", "row"];

    fn parse(f: &Fields<'_>) -> Result<Theorem, InputError> {
        Ok(Theorem {
            name: f.str("name", None)?,
            package: f.one_of("package", &["core", "analysis"], None)?,
            module: f.str("module", None)?,
            says: f.str("says", None)?,
            row: f.str("row", None)?,
        })
    }
}

#[derive(Debug, Clone)]
struct Allow {
    path: String,
    word: String,
}

impl Input for Allow {
    const FIELDS: &'static [&'static str] = &["path", "word", "why"];

    fn parse(f: &Fields<'_>) -> Result<Allow, InputError> {
        let (path, word) = (f.str("path", None)?, f.str("word", None)?);
        // `why` is for people; it is read for its refusals.
        f.str("why", None)?;
        Ok(Allow { path, word })
    }
}

struct Listed {
    axioms: Vec<String>,
    theorems: Vec<Theorem>,
    allow: Vec<Allow>,
}

impl Input for Listed {
    const FIELDS: &'static [&'static str] = &["axioms", "theorems", "allow"];

    fn parse(f: &Fields<'_>) -> Result<Listed, InputError> {
        Ok(Listed { axioms: f.strs_required("axioms")?, theorems: f.objects("theorems", true)?, allow: f.objects("allow", false)? })
    }
}

struct Args {
    command: String,
    root: String,
    lake: String,
    packages: Vec<String>,
    out: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "lake", "packages", "out"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["sources", "check", "write"], None)?,
            root: f.str("root", Some("."))?,
            lake: f.str("lake", Some(""))?,
            packages: f.ones_of("packages", &["core", "analysis"], &["core", "analysis"])?,
            out: f.str("out", Some(""))?,
        })
    }
}

/// Per theorem, the axioms Lean printed for it, in the order first printed (a Python dict).
type Said = Vec<(String, Vec<String>)>;

fn said_set(said: &mut Said, name: &str, axioms: Vec<String>) {
    match said.iter_mut().find(|(it, _)| it == name) {
        Some(slot) => slot.1 = axioms,
        None => said.push((name.to_owned(), axioms)),
    }
}

fn said_get<'s>(said: &'s Said, name: &str) -> Option<&'s Vec<String>> {
    said.iter().find(|(it, _)| it == name).map(|(_, axioms)| axioms)
}

/// Lean source with its comments (`--` to the line's end, `/- .. -/` nested) and the insides of
/// its string literals blanked; newlines stay, so a line number holds.
fn without_comments(text: &str) -> String {
    let text: Vec<char> = text.chars().collect();
    let two = |i: usize| (text.get(i).copied(), text.get(i + 1).copied());
    let mut out = String::with_capacity(text.len());
    let (mut i, mut depth, n) = (0, 0usize, text.len());
    while i < n {
        if depth > 0 {
            if two(i) == (Some('/'), Some('-')) {
                depth += 1;
                i += 2;
            } else if two(i) == (Some('-'), Some('/')) {
                depth -= 1;
                i += 2;
            } else {
                out.push(if text[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        } else if two(i) == (Some('/'), Some('-')) {
            depth = 1;
            i += 2;
        } else if two(i) == (Some('-'), Some('-')) {
            while i < n && text[i] != '\n' {
                i += 1;
            }
        } else if text[i] == '"' {
            out.push('"');
            i += 1;
            while i < n && text[i] != '"' {
                i += if text[i] == '\\' { 2 } else { 1 };
                out.push(' ');
            }
            out.push('"');
            i += 1;
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

fn refused_words(p: &Patterns, path: &str, text: &str, allow: &[Allow]) -> Vec<String> {
    let code = without_comments(text);
    let allowed: BTreeSet<&str> = allow.iter().filter(|a| a.path == path).map(|a| a.word.as_str()).collect();
    let mut out = Vec::new();
    for (word, find) in &p.refused {
        if allowed.contains(word) {
            continue;
        }
        for start in find.starts(&code) {
            let line = code[..start].matches('\n').count() + 1;
            out.push(format!("{path}:{line}: `{word}`: a proof may not say it (allowed only by an entry of {LIST}'s `allow`)"));
        }
    }
    out
}

fn module_path(root: &Path, t: &Theorem) -> Option<PathBuf> {
    let unders = PACKAGES.iter().find(|(name, _, _)| *name == t.package).map_or(&[][..], |(_, _, unders)| unders);
    unders
        .iter()
        .map(|under| root.join(under).join(format!("{}.lean", t.module.replace('.', "/"))))
        .find(|path| path.is_file())
}

/// The statement of `name` as its module states it: from `theorem` up to its `:=` or `where`.
/// The module may state it by a suffix of its full name (inside a namespace); the longest wins.
fn statement(p: &Patterns, text: &str, name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('.').collect();
    for at in 0..parts.len() {
        // Python's `^theorem <short>(?=[\s:({\[])` in multiline mode.
        let needle = format!("theorem {}", parts[at..].join("."));
        let found = text.match_indices(&needle).map(|(start, _)| start).find(|start| {
            let line_start = *start == 0 || text[..*start].ends_with('\n');
            let next = text[start + needle.len()..].chars().next();
            line_start && next.is_some_and(|c| is_space(c) || matches!(c, ':' | '(' | '{' | '['))
        });
        let Some(start) = found else { continue };
        let rest = &text[start..];
        let said = match p.statement_end.find(rest) {
            Some(end) => &rest[..end.start()],
            None => rest,
        };
        return Some(rstrip(said).to_owned());
    }
    None
}

fn lean_sources(root: &Path) -> Vec<String> {
    let shown = root.display().to_string();
    let argv = ["git", "-C", &shown, "ls-files", "-co", "--exclude-standard", "lean/*.lean", "crates/*.lean"];
    let listed = match run(&argv, root) {
        Ok(ran) if ran.status == 0 => universal_newlines(ran.out()),
        Ok(ran) => fail(SCRIPT, &format!("`{}` exited {}: {}", argv.join(" "), ran.status, ran.err().trim())),
        Err(e) => fail(SCRIPT, &e),
    };
    let mut out: Vec<String> = splitlines(&listed).into_iter().filter(|it| !it.contains("/.lake/")).map(str::to_owned).collect();
    out.sort();
    out
}

fn mathlib_rev(lakefile: &str) -> Result<String, String> {
    let data: toml::Table = toml::from_str(lakefile).map_err(|e| e.to_string())?;
    for need in data.get("require").and_then(toml::Value::as_array).into_iter().flatten() {
        if need.get("name").and_then(toml::Value::as_str) == Some("mathlib") {
            return Ok(match need.get("rev") {
                None => String::new(),
                Some(toml::Value::String(rev)) => rev.clone(),
                Some(other) => other.to_string(),
            });
        }
    }
    Ok(String::new())
}

/// Python's name for the type of a JSON value.
fn py_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `value[key]` as Python indexes what `json.loads` gave: its `KeyError` or `TypeError` words.
fn py_index<'v>(value: &'v Value, key: &str) -> Result<&'v Value, String> {
    match value {
        Value::Object(map) => map.get(key).ok_or_else(|| repr_str(key)),
        Value::Array(_) => Err("list indices must be integers or slices, not str".to_owned()),
        Value::String(_) => Err("string indices must be integers, not 'str'".to_owned()),
        other => Err(format!("'{}' object is not subscriptable", py_type(other))),
    }
}

/// Whether Python counts the value false.
fn falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// `{p["name"]: p.get("rev", "") for p in json.loads(manifest)["packages"]}`, or the words of
/// the exception it raised.
fn pinned_of(manifest: &str) -> Result<Vec<(Value, Value)>, String> {
    let data: Value = serde_json::from_str(manifest).map_err(|e| e.to_string())?;
    let packages = py_index(&data, "packages")?;
    let items: Vec<Value> = match packages {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map.keys().map(|k| Value::String(k.clone())).collect(),
        Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        other => return Err(format!("'{}' object is not iterable", py_type(other))),
    };
    let mut pinned: Vec<(Value, Value)> = Vec::new();
    for item in &items {
        let name = py_index(item, "name")?.clone();
        if matches!(name, Value::Array(_) | Value::Object(_)) {
            return Err(format!("unhashable type: '{}'", py_type(&name)));
        }
        let rev = item.get("rev").cloned().unwrap_or_else(|| Value::String(String::new()));
        match pinned.iter_mut().find(|(it, _)| *it == name) {
            Some(slot) => slot.1 = rev,
            None => pinned.push((name, rev)),
        }
    }
    Ok(pinned)
}

/// The checked-in manifest pins Mathlib at the commit lakefile.toml names.
fn manifest_problems(lakefile: &str, manifest: Option<&str>) -> Result<Vec<String>, String> {
    let want = mathlib_rev(lakefile)?;
    if want.is_empty() {
        return Ok(vec!["lean/analysis/lakefile.toml requires no `mathlib` with a `rev`".to_owned()]);
    }
    let Some(manifest) = manifest else {
        return Ok(vec![format!(
            "{MANIFEST} is not checked in: in lean/analysis, with the pinned toolchain, run `lake update` \
             (it resolves Mathlib and its dependencies and writes the file), then commit it"
        )]);
    };
    let pinned = match pinned_of(manifest) {
        Ok(pinned) => pinned,
        Err(e) => return Ok(vec![format!("{MANIFEST} is not a Lake manifest: {e}")]),
    };
    let mathlib = pinned.iter().find(|(name, _)| name.as_str() == Some("mathlib")).map(|(_, rev)| rev);
    if mathlib.and_then(Value::as_str) != Some(want.as_str()) {
        let at = mathlib.map_or_else(|| "None".to_owned(), py_str);
        return Ok(vec![format!(
            "{MANIFEST} pins mathlib at `{at}` and lakefile.toml at `{want}`: run `lake update` in lean/analysis and commit the manifest"
        )]);
    }
    let mut sorted: Vec<(String, &Value)> = pinned.iter().map(|(name, rev)| (py_str(name), rev)).collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(sorted
        .into_iter()
        .filter(|(name, rev)| falsy(rev) && name != "blaze")
        .map(|(name, _)| format!("{MANIFEST}: `{name}` has no pinned commit"))
        .collect())
}

fn list_problems(p: &Patterns, listed: &Listed, rows: Option<&BTreeSet<String>>) -> Vec<String> {
    let mut out = Vec::new();
    let names: Vec<&str> = listed.theorems.iter().map(|t| t.name.as_str()).collect();
    let twice: BTreeSet<&str> = names.iter().copied().filter(|n| names.iter().filter(|it| *it == n).count() > 1).collect();
    out.extend(twice.iter().map(|n| format!("{LIST}: `{n}` is listed twice")));
    for t in &listed.theorems {
        if !p.name.is_match(&t.name) || !p.name.is_match(&t.module) {
            out.push(format!("{LIST}: `{}` in `{}`: a name is dotted identifiers", t.name, t.module));
        }
        if strip(&t.says).is_empty() || t.says.contains('\n') {
            out.push(format!("{LIST}: `{}` says nothing in one line", t.name));
        }
        if let Some(rows) = rows
            && !rows.contains(&t.row)
        {
            out.push(format!("{LIST}: `{}` names the ledger row `{}`, which is no row of {LEDGER}", t.name, t.row));
        }
    }
    if listed.axioms.is_empty() {
        out.push(format!("{LIST}: `axioms` is empty: say which axioms a theorem may rest on"));
    }
    out
}

/// The order of the page's rows: Python's `(r[:1], int(r[1:]) if r[1:].isdigit() else 0)`.
fn row_key(row: &str) -> (String, u128) {
    let first: String = row.chars().take(1).collect();
    let rest = &row[first.len()..];
    let number = if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) { rest.parse().unwrap_or(u128::MAX) } else { 0 };
    (first, number)
}

fn page(listed: &Listed, statements: &[(String, String)]) -> String {
    let mut by_row: Vec<(&str, Vec<&Theorem>)> = Vec::new();
    for t in &listed.theorems {
        match by_row.iter_mut().find(|(row, _)| *row == t.row) {
            Some(slot) => slot.1.push(t),
            None => by_row.push((t.row.as_str(), vec![t])),
        }
    }
    let axioms: Vec<String> = listed.axioms.iter().map(|a| format!("`{a}`")).collect();
    let counts: Vec<String> = PACKAGES
        .iter()
        .map(|(name, dir, _)| format!("{} in `{dir}`", listed.theorems.iter().filter(|t| t.package == *name).count()))
        .collect();
    let mut lines: Vec<String> = vec![
        "---".to_owned(),
        "status: current".to_owned(),
        "summary: Every theorem blaze claims is proved in Lean, with its statement as its module states it and its \
         ledger row; generated by harness/v2/lean_gate.py from lean/theorems.json, and held by the gate to the axioms listed."
            .to_owned(),
        "---".to_owned(),
        "# Proofs".to_owned(),
        String::new(),
        "<!-- Generated by harness/v2/lean_gate.py from lean/theorems.json and the Lean sources: edit those, then run `write`. -->"
            .to_owned(),
        String::new(),
        "What blaze has proved in Lean, by ledger row ([objectives](objectives.md)). The gate".to_owned(),
        "(`harness/v2/lean_gate.py`, command `check`) builds `lean/` and `lean/analysis` with the pinned Lean,".to_owned(),
        "refuses a `sorry` anywhere, and requires each theorem here to exist and to rest on no axiom but".to_owned(),
        format!("{}. What each proof still trusts is in", axioms.join(", ")),
        "[formal-verification.md](design/formal-verification.md) (\"The trusted base\"). A change to this file".to_owned(),
        "in a pull request is a theorem added, restated or dropped.".to_owned(),
        String::new(),
        format!("{} theorems: {}.", listed.theorems.len(), counts.join(", ")),
        String::new(),
    ];
    by_row.sort_by_key(|(row, _)| row_key(row));
    for (row, theorems) in by_row {
        lines.extend([format!("## {row}"), String::new()]);
        for t in theorems {
            let said = statements.iter().find(|(name, _)| *name == t.name).map_or("-- not found in its module", |(_, s)| s.as_str());
            lines.extend([
                format!("**`{}`** (`{}`, `{}`): {}", t.name, t.module, package_dir(&t.package), t.says),
                String::new(),
                "```lean".to_owned(),
                said.to_owned(),
                "```".to_owned(),
                String::new(),
            ]);
        }
    }
    lines.join("\n").trim_end_matches('\n').to_owned() + "\n"
}

fn read_list(root: &Path) -> Listed {
    input::parse::<Listed>(&root.join(LIST).display().to_string(), SCRIPT)
}

/// A file's text, or the script ends naming it (the Python raised there).
fn text_of(path: &Path) -> String {
    read_text(path).unwrap_or_else(|| fail(SCRIPT, &format!("{} could not be read as UTF-8 text", path.display())))
}

/// What is wrong that needs no Lean, and each theorem's statement.
fn sources(p: &Patterns, root: &Path, listed: &Listed) -> (Vec<String>, Vec<(String, String)>) {
    let ledger: Value = serde_json::from_str(&text_of(&root.join(LEDGER)))
        .unwrap_or_else(|e| fail(SCRIPT, &format!("{LEDGER} is not JSON: {e}")));
    let rows: BTreeSet<String> = ledger["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_owned))
        .collect();
    let mut wrong = list_problems(p, listed, Some(&rows));
    let mut statements: Vec<(String, String)> = Vec::new();
    for t in &listed.theorems {
        let said = module_path(root, t).and_then(|path| statement(p, &text_of(&path), &t.name));
        match said {
            None => wrong.push(format!("{LIST}: `{}` is not stated in `{}` ({})", t.name, t.module, package_dir(&t.package))),
            Some(said) => match statements.iter_mut().find(|(name, _)| *name == t.name) {
                Some(slot) => slot.1 = said,
                None => statements.push((t.name.clone(), said)),
            },
        }
    }
    for path in lean_sources(root) {
        wrong.extend(refused_words(p, &path, &text_of(&root.join(&path)), &listed.allow));
    }
    for a in &listed.allow {
        let full = root.join(&a.path);
        let text = if full.is_file() { text_of(&full) } else { String::new() };
        if !p.find(&a.word).is_some_and(|find| find.found_in(&without_comments(&text))) {
            wrong.push(format!("{LIST}: `allow` lets {} say `{}`, which it does not say: remove the entry", a.path, a.word));
        }
    }
    let pins: Vec<(&str, String)> =
        PACKAGES.iter().map(|(name, dir, _)| (*name, strip(&text_of(&root.join(dir).join("lean-toolchain"))).to_owned())).collect();
    if pins.iter().map(|(_, pin)| pin).collect::<BTreeSet<_>>().len() != 1 {
        let shown: Vec<String> = pins.iter().map(|(name, pin)| format!("{}: {}", repr_str(name), repr_str(pin))).collect();
        wrong.push(format!("the packages pin different toolchains: {{{}}}", shown.join(", ")));
    }
    let manifest = root.join(MANIFEST);
    let manifest = if manifest.is_file() { Some(text_of(&manifest)) } else { None };
    match manifest_problems(&text_of(&root.join("lean/analysis/lakefile.toml")), manifest.as_deref()) {
        Ok(problems) => wrong.extend(problems),
        Err(e) => fail(SCRIPT, &format!("lean/analysis/lakefile.toml is not TOML: {e}")),
    }
    let page_path = root.join(PAGE);
    if !page_path.is_file() || read_text(&page_path).as_deref() != Some(page(listed, &statements).as_str()) {
        wrong.push(format!("{PAGE} is stale: run {SCRIPT} with command `write` and commit it"));
    }
    (wrong, statements)
}

/// The Lean file that asks, per theorem, which axioms it rests on.
fn axioms_file(theorems: &[&Theorem]) -> String {
    let modules: BTreeSet<&str> = theorems.iter().map(|t| t.module.as_str()).collect();
    let mut lines: Vec<String> = modules.iter().map(|m| format!("import {m}")).collect();
    lines.push(String::new());
    lines.extend(theorems.iter().map(|t| format!("#print axioms {}", t.name)));
    lines.join("\n") + "\n"
}

/// Per theorem, the axioms Lean printed for it.
fn axioms_said(p: &Patterns, output: &str) -> Said {
    let mut said = Said::new();
    for found in p.axioms.captures_iter(output) {
        let axioms = found[2].split(',').map(strip).filter(|a| !a.is_empty()).map(str::to_owned).collect();
        said_set(&mut said, &found[1], axioms);
    }
    for found in p.no_axioms.captures_iter(output) {
        said_set(&mut said, &found[1], Vec::new());
    }
    said
}

fn axiom_problems(theorems: &[&Theorem], said: &Said, allowed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for t in theorems {
        let Some(got) = said_get(said, &t.name) else {
            out.push(format!(
                "`{}`: Lean printed no axioms for it: it does not exist in `{}`, or the module did not load",
                t.name, t.module
            ));
            continue;
        };
        let extra: Vec<&str> = got.iter().map(String::as_str).filter(|a| !allowed.iter().any(|it| it == a)).collect();
        if !extra.is_empty() {
            let named: Vec<String> = extra.iter().map(|a| format!("`{a}`")).collect();
            let sorry = if extra.contains(&"sorryAx") { ": it is not proved (`sorryAx` is a `sorry`)" } else { "" };
            out.push(format!("`{}` rests on {}, beyond {}{sorry}", t.name, named.join(", "), allowed.join(", ")));
        }
    }
    out
}

/// The last `n` characters of `text`, as Python's `text[-n:]`.
fn tail(text: &str, n: usize) -> &str {
    let count = text.chars().count();
    if count <= n {
        return text;
    }
    let skip = text.char_indices().nth(count - n).map_or(text.len(), |(at, _)| at);
    &text[skip..]
}

fn lake_run(argv: &[String], cwd: &Path) -> (i32, String) {
    let start = Instant::now();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let done = run(&args, cwd).unwrap_or_else(|e| fail(SCRIPT, &e));
    let took = start.elapsed().as_secs_f64();
    println!("{SCRIPT}: `{}` in {}: exit {}, {took:.0} s", args[1..].join(" "), cwd.display(), done.status);
    (done.status, universal_newlines(done.out()) + &universal_newlines(done.err()))
}

fn build(root: &Path, lake: &Path, package: &str) -> Vec<String> {
    let dir = package_dir(package);
    let cwd = root.join(dir);
    let lake = lake.display().to_string();
    let mut steps = vec![vec![lake.clone(), "build".to_owned()]];
    if package == "analysis" {
        steps.insert(0, vec![lake.clone(), "exe".to_owned(), "cache".to_owned(), "get".to_owned()]);
    }
    for argv in steps {
        let (code, log) = lake_run(&argv, &cwd);
        if code != 0 {
            return vec![format!("`lake {}` in {dir} failed ({code}):\n{}", argv[1..].join(" "), tail(&log, 6000))];
        }
        if log.contains(SORRY) {
            let lines: Vec<&str> = splitlines(&log).into_iter().filter(|line| line.contains(SORRY)).collect();
            let shown: Vec<&str> = lines.iter().take(20).copied().collect();
            return vec![format!("`lake build` in {dir}: {} declarations use `sorry`:\n{}", lines.len(), shown.join("\n"))];
        }
    }
    Vec::new()
}

fn axioms(p: &Patterns, root: &Path, lake: &Path, package: &str, listed: &Listed) -> (Vec<String>, Said) {
    let theorems: Vec<&Theorem> = listed.theorems.iter().filter(|t| t.package == package).collect();
    if theorems.is_empty() {
        return (Vec::new(), Said::new());
    }
    let (code, log) = {
        let tmp = TempDir::new("lean-gate-").unwrap_or_else(|e| fail(SCRIPT, &e));
        let file = tmp.path().join("Axioms.lean");
        if let Err(e) = std::fs::write(&file, axioms_file(&theorems)) {
            fail(SCRIPT, &blaze_script::os_error(&e, &file.display().to_string()));
        }
        let argv = [lake.display().to_string(), "env".to_owned(), "lean".to_owned(), file.display().to_string()];
        lake_run(&argv, &root.join(package_dir(package)))
    };
    let said = axioms_said(p, &log);
    let mut wrong = axiom_problems(&theorems, &said, &listed.axioms);
    if !wrong.is_empty() && code != 0 {
        wrong.push(format!("Lean said, of {}'s axioms file:\n{}", package_dir(package), tail(&log, 3000)));
    }
    (wrong, said)
}

fn table(listed: &Listed, said: &Said) -> String {
    let width = listed.theorems.iter().map(|t| t.name.chars().count()).max().unwrap_or(0);
    let mut rows = vec![format!("{:<width$}  row  axioms", "theorem")];
    for t in &listed.theorems {
        let got = match said_get(said, &t.name) {
            None => "not asked".to_owned(),
            Some(got) if got.is_empty() => "none".to_owned(),
            Some(got) => got.join(", "),
        };
        rows.push(format!("{:<width$}  {:<3}  {got}", t.name, t.row));
    }
    rows.join("\n")
}

/// mir_semantics.py's `lake_of`: the pinned toolchain's lake, `given` or the one elan keeps for
/// lean/lean-toolchain's pin. Its refusals carry that script's name, as the Python's did.
fn lake_of(given: &str, root: &Path, home: &Path) -> PathBuf {
    const OWNER: &str = "mir_semantics.py";
    if !given.is_empty() {
        if !given.starts_with('/') {
            fail(OWNER, &format!("`lake` is `{given}`: give the pinned toolchain's lake by an absolute path"));
        }
        return PathBuf::from(given);
    }
    let pin = strip(&text_of(&root.join("lean/lean-toolchain"))).to_owned();
    let lake = home.join(".elan/toolchains").join(pin.replace('/', "--").replace(':', "---")).join("bin/lake");
    if !lake.is_file() {
        fail(
            OWNER,
            &format!(
                "no `{}` (the toolchain lean/lean-toolchain pins, `{pin}`): install it, or give `lake` by an absolute path",
                lake.display()
            ),
        );
    }
    lake
}

fn main_with(p: &Patterns, it: &Args) {
    // `Path(root).resolve()`: a root that is not there is found out when the list is read.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let listed = read_list(&root);
    let (mut wrong, statements) = sources(p, &root, &listed);
    if it.command == "write" {
        let path = root.join(PAGE);
        if let Err(e) = std::fs::write(&path, page(&listed, &statements)) {
            fail(SCRIPT, &blaze_script::os_error(&e, &path.display().to_string()));
        }
        return;
    }
    let mut said = Said::new();
    if it.command == "check" && wrong.is_empty() {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let lake = lake_of(&it.lake, &root, &home);
        for package in &it.packages {
            wrong.extend(build(&root, &lake, package));
            if !wrong.is_empty() {
                break;
            }
            let (its, heard) = axioms(p, &root, &lake, package, &listed);
            wrong.extend(its);
            for (name, axioms) in heard {
                said_set(&mut said, &name, axioms);
            }
        }
        println!("{}", table(&listed, &said));
    }
    if !it.out.is_empty() {
        let detail = wrong.first().map_or("", |w| splitlines(w).first().copied().unwrap_or(""));
        let verdict = if wrong.is_empty() { "pass" } else { "fail" };
        let value = json!({"outcomes": [{"name": "lean-gate", "verdict": verdict, "detail": detail}]});
        if let Err(e) = std::fs::write(&it.out, dumps(&value) + "\n") {
            fail(SCRIPT, &blaze_script::os_error(&e, &it.out));
        }
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    let asked = if it.command == "sources" {
        "sources hold".to_owned()
    } else {
        format!("{} theorems rest on the allowed axioms alone", said.len())
    };
    println!("{SCRIPT}: ok: {asked}");
}

// The self-test.

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// `repr` of an optional string, as the selftest's messages print it.
fn repr_opt(it: Option<&str>) -> String {
    it.map_or_else(|| "None".to_owned(), repr_str)
}

/// `repr` of a `Said`, a dict of tuples.
fn repr_said(said: &Said) -> String {
    let items: Vec<String> = said
        .iter()
        .map(|(name, axioms)| {
            let inner: Vec<String> = axioms.iter().map(|a| repr_str(a)).collect();
            let tuple = if inner.len() == 1 { format!("({},)", inner[0]) } else { format!("({})", inner.join(", ")) };
            format!("{}: {tuple}", repr_str(name))
        })
        .collect();
    format!("{{{}}}", items.join(", "))
}

fn theorem(name: &str, package: &str, module: &str, says: &str, row: &str) -> Theorem {
    Theorem { name: name.to_owned(), package: package.to_owned(), module: module.to_owned(), says: says.to_owned(), row: row.to_owned() }
}

fn problems_or_fail(it: Result<Vec<String>, String>) -> Vec<String> {
    it.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e))
}

fn selftest(p: &Patterns) {
    let code = without_comments("def a := 1 -- sorry here\n/- sorry /- nested sorry -/ still -/ def b := \"sorry \\\" sorry\"\nexact x\n");
    expect(
        !code.contains("sorry") && code.contains("def b") && code.matches('\n').count() == 3,
        &format!("comments and strings are blanked: {}", repr_str(&code)),
    );
    let none: &[Allow] = &[];
    expect(
        refused_words(p, "A.lean", "theorem t : True := by\n  sorry\n", none).first().is_some_and(|w| w.starts_with("A.lean:2: `sorry`")),
        "a sorry is refused, by line",
    );
    expect(
        refused_words(p, "A.lean", "-- sorry\n/-- no sorry -/\ntheorem t : True := trivial\n", none).is_empty(),
        "a sorry in a comment is not one",
    );
    expect(
        refused_words(p, "A.lean", "theorem sorry_free : True := trivial\ndef x := Foo.sorry'\n", none).is_empty(),
        "a name holding the word is not one",
    );
    for (text, word) in [
        ("example : 1 = 1 := by native_decide\n", "native_decide"),
        ("example : 1 = 1 := by decide +native\n", "decide +native"),
        ("axiom choice : False\n", "axiom"),
        ("private axiom c : False\n", "axiom"),
        ("partial def f : Nat := f\n", "partial def"),
        ("example : True := by admit\n", "admit"),
        ("set_option debug.skipKernelTC true\n", "debug.skipKernelTC"),
    ] {
        expect(
            refused_words(p, "A.lean", text, none).iter().any(|w| w.contains(&format!("`{word}`"))),
            &format!("`{word}` is refused in {}", repr_str(text)),
        );
    }
    expect(
        refused_words(p, "A.lean", "theorem t (h : axiom_free) : True := trivial\n", none).is_empty(),
        "`axiom` inside a line is not a declaration",
    );
    let allow = [Allow { path: "A.lean".to_owned(), word: "partial def".to_owned() }];
    expect(refused_words(p, "A.lean", "partial def f : Nat := f\n", &allow).is_empty(), "an allowed word in its file passes");
    expect(!refused_words(p, "B.lean", "partial def f : Nat := f\n", &allow).is_empty(), "and in no other file");

    let text = "namespace Blaze.Mir\n\n/-- Doc. -/\ntheorem run_mono (prog : Program) :\n    ∀ (n k : Nat), run prog (n + k) = r := by\n  intro n\n\n\
                theorem State.read_write_other (st : State) : a = b := rfl\n\ntheorem run_monotone : True := trivial\n\
                theorem sound : Rewrite.AllSound where\n  a := b\n";
    let mono = statement(p, text, "Blaze.Mir.run_mono");
    expect(
        mono.as_deref() == Some("theorem run_mono (prog : Program) :\n    ∀ (n k : Nat), run prog (n + k) = r"),
        &repr_opt(mono.as_deref()),
    );
    expect(
        statement(p, text, "Blaze.Mir.State.read_write_other").as_deref() == Some("theorem State.read_write_other (st : State) : a = b"),
        "a dotted suffix",
    );
    expect(
        statement(p, text, "Blaze.Mir.sound").as_deref() == Some("theorem sound : Rewrite.AllSound"),
        "a structure instance ends at `where`",
    );
    expect(statement(p, text, "Blaze.Mir.run_mon").is_none(), "a prefix of another name is not it");

    let theorems = [
        theorem("Blaze.Mir.run_mono", "core", "Blaze.Mir.Run", "a run that ends, ends the same with more fuel", "V6"),
        theorem("Blaze.Mir.Opt.validate_sound", "core", "Blaze.Mir.Opt.Validate", "an accepted pair runs alike", "V8"),
    ];
    let refs: Vec<&Theorem> = theorems.iter().collect();
    let axioms_allowed: Vec<String> = ["propext", "Classical.choice", "Quot.sound"].map(str::to_owned).to_vec();
    let listed = Listed { axioms: axioms_allowed.clone(), theorems: theorems.to_vec(), allow: Vec::new() };
    let file = axioms_file(&refs);
    expect(
        file == "import Blaze.Mir.Opt.Validate\nimport Blaze.Mir.Run\n\n#print axioms Blaze.Mir.run_mono\n#print axioms Blaze.Mir.Opt.validate_sound\n",
        &file,
    );
    let said = axioms_said(
        p,
        "'Blaze.Mir.run_mono' depends on axioms: [propext,\n Classical.choice]\n'Blaze.Mir.Opt.validate_sound' does not depend on any axioms\n",
    );
    let mut sorted = said.clone();
    sorted.sort();
    expect(
        sorted
            == [
                ("Blaze.Mir.Opt.validate_sound".to_owned(), Vec::new()),
                ("Blaze.Mir.run_mono".to_owned(), vec!["propext".to_owned(), "Classical.choice".to_owned()]),
            ],
        &repr_said(&said),
    );
    expect(axiom_problems(&refs, &said, &listed.axioms).is_empty(), "the three axioms and none are allowed");
    let sorried = axioms_said(p, "'Blaze.Mir.run_mono' depends on axioms: [sorryAx]\n");
    let wrong = axiom_problems(&refs, &sorried, &listed.axioms);
    expect(
        wrong.len() == 2 && wrong[0].contains("`sorryAx`") && wrong[1].contains("printed no axioms"),
        &format!("a sorry and a missing theorem: {}", repr_strs(&wrong)),
    );
    let native = axioms_said(p, "'Blaze.Mir.run_mono' depends on axioms: [propext, Lean.ofReduceBool]\n");
    expect(
        axiom_problems(&refs[..1], &native, &listed.axioms).first().is_some_and(|w| w.contains("`Lean.ofReduceBool`")),
        "native evaluation is refused",
    );
    let shown = table(&listed, &said);
    expect(shown.contains("Blaze.Mir.run_mono") && shown.contains("none"), &shown);

    let rows = |ids: &[&str]| ids.iter().map(|it| (*it).to_owned()).collect::<BTreeSet<String>>();
    let both = list_problems(p, &listed, Some(&rows(&["V6", "V8"])));
    expect(both.is_empty(), &repr_strs(&both));
    expect(list_problems(p, &listed, Some(&rows(&["V6"]))).iter().any(|w| w.contains("no row")), "an unknown ledger row is a problem");
    let twice = Listed { axioms: axioms_allowed.clone(), theorems: vec![theorems[0].clone(), theorems[0].clone()], allow: Vec::new() };
    expect(list_problems(p, &twice, None).iter().any(|w| w.contains("listed twice")), "a theorem listed twice");
    let bare = Listed { axioms: Vec::new(), theorems: theorems.to_vec(), allow: Vec::new() };
    expect(list_problems(p, &bare, None).iter().any(|w| w.contains("`axioms` is empty")), "no allowed axioms is a problem");

    let lakefile = "[[require]]\nname = \"mathlib\"\ngit = \"https://x\"\nrev = \"abc\"\n\n[[require]]\nname = \"blaze\"\npath = \"..\"\n";
    let rev = mathlib_rev(lakefile).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(rev == "abc", &rev);
    let first = |problems: Vec<String>, says: &str| problems.first().is_some_and(|w| w.contains(says));
    expect(first(problems_or_fail(manifest_problems(lakefile, None)), "is not checked in"), "a missing manifest is a problem");
    let good = dumps(&json!({"packages": [{"name": "mathlib", "rev": "abc"}, {"name": "batteries", "rev": "d"}, {"name": "blaze", "type": "path"}]}));
    let held = problems_or_fail(manifest_problems(lakefile, Some(&good)));
    expect(held.is_empty(), &repr_strs(&held));
    let moved = dumps(&json!({"packages": [{"name": "mathlib", "rev": "abd"}]}));
    expect(
        first(problems_or_fail(manifest_problems(lakefile, Some(&moved))), "pins mathlib at `abd`"),
        "a manifest at another commit is a problem",
    );
    expect(first(problems_or_fail(manifest_problems(lakefile, Some("{}"))), "not a Lake manifest"), "a file that is no manifest is a problem");

    let text = page(&listed, &[("Blaze.Mir.run_mono".to_owned(), "theorem run_mono : True".to_owned())]);
    expect(
        text.contains("## V6\n\n**`Blaze.Mir.run_mono`** (`Blaze.Mir.Run`, `lean`): a run that ends")
            && text.contains("```lean\ntheorem run_mono : True\n```"),
        &text,
    );
    expect(text.contains("2 theorems: 2 in `lean`, 0 in `lean/analysis`.") && text.contains("-- not found in its module"), &text);
    let given = input::parse::<Args>(r#"{"command": "check", "packages": ["core"]}"#, SCRIPT);
    let packages: Vec<String> = given.packages.iter().map(|it| repr_str(it)).collect();
    let tuple = if packages.len() == 1 { format!("({},)", packages[0]) } else { format!("({})", packages.join(", ")) };
    expect(
        given.root == "." && given.lake.is_empty() && given.packages == ["core"],
        &format!(
            "the input: Input(command={}, root={}, lake={}, packages={tuple}, out={})",
            repr_str(&given.command),
            repr_str(&given.root),
            repr_str(&given.lake),
            repr_str(&given.out)
        ),
    );
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
