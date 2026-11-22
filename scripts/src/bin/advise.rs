//! advise: one ranked list for an agent deciding what to fix next, from every ratchet that knows
//! its sites (docs/v2/plan/design/std.md, "What an author that does not tire can write": the
//! first form of the tool telling the author where the explicit form pays). `DOC` below is what
//! its usage error prints; it says what is read, how it is ranked and the schema.
//!
//!     blaze run -q -p scripts --bin advise -- '{}'
//!     blaze run -q -p scripts --bin advise -- '{"crate": "blaze-hash", "format": "json"}'
//!     blaze run -q -p scripts --bin advise -- selftest
//!
//! It runs each ratchet's `{"command": "findings"}` (the binaries beside its own, built with it),
//! refines `ambient`'s lines by tools/std-map.json, ranks, and prints. The schema of what it reads
//! is `blaze_script::finding`'s; what it prints is [`SCHEMA`].

/// What the usage error prints.
const DOC: &str = r#"advise: one ranked list of what to fix next, from every ratchet's findings.

    blaze run -q -p scripts --bin advise -- '{"crate": null, "format": "table", "top": 25}'
    blaze run -q -p scripts --bin advise -- selftest

    root     the checkout (".")
    crate    one crate (its directory under crates/), or null for the workspace
    format   table   a line for the workspace and one per crate, most to fix first:
                     "crate X: 41 mechanical std renames, 12 bare casts, 3 direct recursions"
             json    one document, `blaze-advice/1` (below), every finding ranked
    top      how many crates the table shows (25); the document holds them all

It runs, beside its own binary, `ambient`, `allocs`, `casts`, `clones`, `recursion`, `stops` and
`untyped` with `{"command": "findings", "root": <root>}`, 300 s each at most, and stops naming
the one that is missing, fails, runs over or prints no `blaze-findings/1` document. `thirdparty`
has no `findings` command yet: the document says so under `not_run`.

A finding's kind is `<ratchet>.<kind>`. An `ambient` line is refined by tools/std-map.json: each
row whose `written` form (or, for a path, the path less its root crate) is on the line, longest
first and not overlapping, is one finding `ambient.<class>` (rename, add-arg, thread, no-form,
missing) whose `instead` is the row's replacement and `detail` its path; a line no row matches
stays `ambient.<kind>`. Mechanical (a rewrite from the text alone, tools/tobstd.rs's): the
`rename` and `add-arg` classes and `clones.handle`.

Rank: mechanical first, then the count of its crate's findings of its kind (most first), then
kind, crate, file and line. `weight` is that order as one number: 2^32 when mechanical, plus
the count. Equal trees print equal bytes.

The document:
  {"schema": "blaze-advice/1", "crate": <null or name>,
   "sources": [{"source", "findings"}], "not_run": [{"source", "why"}],
   "kinds": [{"kind", "label", "rule", "instead", "mechanical", "count"}],
   "measured_kinds": [{"kind", "what"}],
   "findings": [{"rank", "kind", "crate", "file", "line", "rule", "instead", "mechanical",
                 "weight", "detail", "measured"}]}
`measured` is null on every finding today: it is the place for what a profile or a test run
measures at the site, and `measured_kinds` names the kinds it will hold: measured.hot_call (the
site's share of a profiled build's time), measured.instantiations (monomorphized copies of the
function it is in), measured.bounds_check (index checks the optimizer kept, at run time),
measured.alloc_bytes (bytes the site allocated in a profiled run). A measured weight then ranks
above a count.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    Map, Value, fail, fan_out,
    finding::SCHEMA as FINDINGS,
    input::{self, Fields, Input, InputError},
    json, read_text, run_within,
};

const SCRIPT: &str = "advise";
/// What this prints, with `format` json.
const SCHEMA: &str = "blaze-advice/1";
/// The ratchets with a `findings` command, run in this order.
const SOURCES: [&str; 7] = ["ambient", "allocs", "casts", "clones", "recursion", "stops", "untyped"];
/// The ratchets with sites and no `findings` command yet, and why.
const NOT_RUN: [(&str, &str); 1] = [(
    "thirdparty",
    "no `findings` command yet: its script was being rewritten by another lane when this was written; add one as in casts.rs",
)];
/// Each ratchet's time to answer.
const LIMIT: Duration = Duration::from_secs(300);
const STD_MAP: &str = "tools/std-map.json";
/// The std-map classes, their labels, and whether their rewrite is mechanical (tobstd.rs's).
const CLASSES: [(&str, &str, bool); 5] = [
    ("rename", "mechanical std renames", true),
    ("add-arg", "std uses that take an allocator argument", true),
    ("thread", "std uses that need a capability passed in", false),
    ("no-form", "std uses with no bstd form yet", false),
    ("missing", "std uses blaze-own lacks", false),
];
const AMBIENT_RULE: &str = "75 nothing-ambient";
/// Kinds of other ratchets whose fix is mechanical, though their ratchet says otherwise.
const MECHANICAL: [&str; 1] = ["clones.handle"];
/// What `measured` will hold.
const MEASURED: [(&str, &str); 4] = [
    ("measured.hot_call", "the site's share of a profiled build's time"),
    ("measured.instantiations", "the monomorphized copies of the function the site is in"),
    ("measured.bounds_check", "the index checks the optimizer kept at the site, at run time"),
    ("measured.alloc_bytes", "the bytes the site allocated in a profiled run"),
];

struct Args {
    root: String,
    krate: Option<String>,
    format: String,
    top: usize,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["root", "crate", "format", "top"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let top = f.int("top", Some(25))?;
        Ok(Args {
            root: f.str("root", Some("."))?,
            krate: f.str_or_null("crate")?,
            format: f.one_of("format", &["table", "json"], Some("table"))?,
            top: usize::try_from(top).map_err(|_| InputError(format!("{} is {top}: a count of crates, 0 or more", f.at("top"))))?,
        })
    }
}

/// A kind as advise ranks it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KindRow {
    /// `<ratchet>.<kind>`.
    kind: String,
    label: String,
    rule: String,
    instead: String,
    mechanical: bool,
}

/// One site as a ratchet reported it, its kind made `<ratchet>.<kind>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    krate: String,
    file: String,
    line: usize,
    kind: String,
    detail: Option<String>,
    /// Its own `instead` when it has one (a std-map row's replacement), else its kind's.
    instead: Option<String>,
}

/// One ratchet's document.
#[derive(Debug, Default)]
struct Doc {
    source: String,
    kinds: Vec<KindRow>,
    sites: Vec<Site>,
}

/// A ratchet's document wrong: which ratchet, and what.
fn wrong(source: &str, what: &str) -> String {
    format!("`{source}`'s findings document {what} (it is `{FINDINGS}`, blaze_script::finding)")
}

/// `source`'s document from its text.
fn parse_doc(source: &str, text: &str) -> Result<Doc, String> {
    let value: Value = json::from_str(text.trim()).map_err(|e| wrong(source, &format!("is not JSON: {e}")))?;
    let field = |it: &Value, name: &str| it.get(name).and_then(Value::as_str).map(str::to_owned);
    if field(&value, "schema").as_deref() != Some(FINDINGS) {
        return Err(wrong(source, "says no schema, or another one"));
    }
    let mut doc = Doc { source: source.to_owned(), ..Doc::default() };
    for kind in value.get("kinds").and_then(Value::as_array).ok_or_else(|| wrong(source, "has no `kinds`"))? {
        let (Some(name), Some(label), Some(rule), Some(instead), Some(mechanical)) = (
            field(kind, "name"),
            field(kind, "label"),
            field(kind, "rule"),
            field(kind, "instead"),
            kind.get("mechanical").and_then(Value::as_bool),
        ) else {
            return Err(wrong(source, &format!("has a kind that is not `{{name, label, rule, instead, mechanical}}`: {kind}")));
        };
        let kind = format!("{source}.{name}");
        let mechanical = mechanical || MECHANICAL.contains(&kind.as_str());
        doc.kinds.push(KindRow { kind, label, rule, instead, mechanical });
    }
    for it in value.get("findings").and_then(Value::as_array).ok_or_else(|| wrong(source, "has no `findings`"))? {
        let line = it.get("line").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
        let (Some(krate), Some(file), Some(line), Some(kind)) = (field(it, "crate"), field(it, "file"), line, field(it, "kind")) else {
            return Err(wrong(source, &format!("has a finding that is not `{{crate, file, line, kind, detail}}`: {it}")));
        };
        let kind = format!("{source}.{kind}");
        if !doc.kinds.iter().any(|k| k.kind == kind) {
            return Err(wrong(source, &format!("has a finding of `{kind}`, which its `kinds` do not name")));
        }
        doc.sites.push(Site { krate, file, line, kind, detail: field(it, "detail"), instead: None });
    }
    Ok(doc)
}

/// A row of tools/std-map.json, as the refinement reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StdRow {
    path: String,
    written: String,
    class: String,
    replacement: String,
}

impl StdRow {
    /// The texts that name it on a line: its written form, and for a path its path less the root
    /// crate (`collections::HashMap` of `std::collections::HashMap`, as a grouped import writes it).
    fn forms(&self) -> Vec<&str> {
        let mut out = vec![self.written.as_str()];
        if let Some((_, rest)) = self.path.split_once("::") {
            if rest.contains("::") && rest != self.written {
                out.push(rest);
            }
        }
        out
    }
}

fn parse_std_map(text: &str) -> Result<Vec<StdRow>, String> {
    let value: Value = json::from_str(text).map_err(|e| format!("{STD_MAP} is not JSON: {e}"))?;
    let rows = value.get("rows").and_then(Value::as_array).ok_or_else(|| format!("{STD_MAP} has no `rows`"))?;
    let mut out = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        let field = |name: &str| row.get(name).and_then(Value::as_str).map(str::to_owned);
        let (Some(path), Some(written), Some(class), Some(replacement)) = (field("path"), field("written"), field("class"), field("replacement")) else {
            return Err(format!("{STD_MAP}: row {at} has no `path`, `written`, `class` or `replacement`"));
        };
        if !CLASSES.iter().any(|(name, ..)| *name == class) {
            return Err(format!("{STD_MAP}: row {at} (`{path}`) has the class `{class}`, which advise does not know: add it to CLASSES"));
        }
        out.push(StdRow { path, written, class, replacement });
    }
    Ok(out)
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The std-map rows named on `line`: longest form first, matches not overlapping, a form that
/// starts or ends with a word character matched only as a whole word; in line order.
fn rows_on<'r>(line: &str, by_length: &[(&str, &'r StdRow)]) -> Vec<&'r StdRow> {
    let bytes = line.as_bytes();
    let mut taken = vec![false; bytes.len()];
    let mut out: Vec<(usize, &StdRow)> = Vec::new();
    for (form, row) in by_length {
        let mut from = 0;
        // Bounded: each turn moves `from` past a match.
        while let Some(found) = line[from..].find(form) {
            let at = from + found;
            let end = at + form.len();
            from = at + 1;
            let first = form.as_bytes()[0];
            let last = form.as_bytes()[form.len() - 1];
            let open = !is_ident(first) || at == 0 || !(is_ident(bytes[at - 1]) || bytes[at - 1] == b':');
            let close = !is_ident(last) || end == bytes.len() || !is_ident(bytes[end]);
            if open && close && !taken[at..end].contains(&true) {
                taken[at..end].iter_mut().for_each(|it| *it = true);
                out.push((at, *row));
            }
        }
    }
    out.sort_by_key(|(at, _)| *at);
    out.into_iter().map(|(_, row)| row).collect()
}

/// The refined `ambient` document: each line's std-map rows as findings of their class, a line
/// no row matches kept as it was. `text_of` gives a file's text, from the root.
fn refine(doc: Doc, map: &[StdRow], mut text_of: impl FnMut(&str) -> Option<String>) -> Doc {
    let mut forms: Vec<(&str, &StdRow)> = map.iter().flat_map(|row| row.forms().into_iter().map(move |form| (form, row))).collect();
    forms.retain(|(form, _)| !form.is_empty());
    forms.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)).then(a.1.path.cmp(&b.1.path)));
    let mut kinds: Vec<KindRow> = CLASSES
        .iter()
        .map(|(class, label, mechanical)| KindRow {
            kind: format!("ambient.{class}"),
            label: (*label).to_owned(),
            rule: AMBIENT_RULE.to_owned(),
            instead: "the row's replacement in tools/std-map.json".to_owned(),
            mechanical: *mechanical,
        })
        .collect();
    kinds.extend(doc.kinds);
    // One line once, whatever ambient's kinds on it.
    let mut lines: BTreeMap<(String, String, usize), Vec<Site>> = BTreeMap::new();
    for site in doc.sites {
        lines.entry((site.krate.clone(), site.file.clone(), site.line)).or_default().push(site);
    }
    let mut files: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    let mut sites = Vec::new();
    for ((krate, file, line), on) in lines {
        let text = files.entry(file.clone()).or_insert_with(|| text_of(&file).map(|t| t.split('\n').map(str::to_owned).collect()));
        let rows = text.as_ref().and_then(|t| t.get(line - 1)).map(|it| rows_on(it, &forms)).unwrap_or_default();
        if rows.is_empty() {
            sites.extend(on);
            continue;
        }
        for row in rows {
            sites.push(Site {
                krate: krate.clone(),
                file: file.clone(),
                line,
                kind: format!("ambient.{}", row.class),
                detail: Some(row.path.clone()),
                instead: Some(format!("`{}`", row.replacement)),
            });
        }
    }
    Doc { source: doc.source, kinds, sites }
}

/// A ranked finding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ranked {
    site: Site,
    mechanical: bool,
    /// Its crate's findings of its kind.
    count: usize,
}

impl Ranked {
    fn weight(&self) -> u64 {
        (u64::from(self.mechanical) << 32) + u64::try_from(self.count).unwrap_or(u64::from(u32::MAX))
    }
}

/// Every site of `docs`, of `only` crate when given, ranked.
fn rank(docs: &[Doc], only: Option<&str>) -> (BTreeMap<String, KindRow>, Vec<Ranked>) {
    let kinds: BTreeMap<String, KindRow> = docs.iter().flat_map(|d| d.kinds.iter().map(|k| (k.kind.clone(), k.clone()))).collect();
    let sites: Vec<&Site> = docs.iter().flat_map(|d| d.sites.iter()).filter(|s| only.is_none_or(|it| s.krate == it)).collect();
    let mut groups: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for site in &sites {
        *groups.entry((site.krate.as_str(), site.kind.as_str())).or_default() += 1;
    }
    let mut out: Vec<Ranked> = sites
        .iter()
        .map(|site| Ranked {
            site: (*site).clone(),
            mechanical: kinds.get(&site.kind).is_some_and(|k| k.mechanical),
            count: groups.get(&(site.krate.as_str(), site.kind.as_str())).copied().unwrap_or(0),
        })
        .collect();
    out.sort_by(|a, b| {
        b.weight().cmp(&a.weight()).then_with(|| (&a.site.kind, &a.site.krate, &a.site.file, a.site.line, &a.site.detail).cmp(&(&b.site.kind, &b.site.krate, &b.site.file, b.site.line, &b.site.detail)))
    });
    (kinds, out)
}

/// The document `format` json prints.
fn document(only: Option<&str>, docs: &[Doc], kinds: &BTreeMap<String, KindRow>, ranked: &[Ranked]) -> Value {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for it in ranked {
        *counts.entry(it.site.kind.as_str()).or_default() += 1;
    }
    let object = |pairs: Vec<(&str, Value)>| Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect::<Map<String, Value>>());
    let sources = docs.iter().map(|d| object(vec![("source", Value::from(d.source.as_str())), ("findings", Value::from(d.sites.len()))])).collect();
    let not_run = NOT_RUN.iter().map(|(source, why)| object(vec![("source", Value::from(*source)), ("why", Value::from(*why))])).collect();
    let kind_rows = kinds
        .values()
        .map(|k| {
            object(vec![
                ("kind", Value::from(k.kind.as_str())),
                ("label", Value::from(k.label.as_str())),
                ("rule", Value::from(k.rule.as_str())),
                ("instead", Value::from(k.instead.as_str())),
                ("mechanical", Value::from(k.mechanical)),
                ("count", Value::from(counts.get(k.kind.as_str()).copied().unwrap_or(0))),
            ])
        })
        .collect();
    let measured = MEASURED.iter().map(|(kind, what)| object(vec![("kind", Value::from(*kind)), ("what", Value::from(*what))])).collect();
    let findings = ranked
        .iter()
        .enumerate()
        .map(|(at, it)| {
            let kind = kinds.get(&it.site.kind);
            let instead = it.site.instead.clone().or_else(|| kind.map(|k| k.instead.clone())).unwrap_or_default();
            object(vec![
                ("rank", Value::from(at + 1)),
                ("kind", Value::from(it.site.kind.as_str())),
                ("crate", Value::from(it.site.krate.as_str())),
                ("file", Value::from(it.site.file.as_str())),
                ("line", Value::from(it.site.line)),
                ("rule", Value::from(kind.map_or("", |k| k.rule.as_str()))),
                ("instead", Value::from(instead)),
                ("mechanical", Value::from(it.mechanical)),
                ("weight", Value::from(it.weight())),
                ("detail", it.site.detail.clone().map_or(Value::Null, Value::from)),
                ("measured", Value::Null),
            ])
        })
        .collect();
    object(vec![
        ("schema", Value::from(SCHEMA)),
        ("crate", only.map_or(Value::Null, Value::from)),
        ("sources", Value::Array(sources)),
        ("not_run", Value::Array(not_run)),
        ("kinds", Value::Array(kind_rows)),
        ("measured_kinds", Value::Array(measured)),
        ("findings", Value::Array(findings)),
    ])
}

/// `41 mechanical std renames, 12 bare casts`: each kind of `ranked` once, in rank order.
fn phrase(kinds: &BTreeMap<String, KindRow>, ranked: &[&Ranked]) -> String {
    let mut seen: Vec<(&str, usize)> = Vec::new();
    for it in ranked {
        match seen.iter_mut().find(|(kind, _)| *kind == it.site.kind) {
            Some(row) => row.1 += 1,
            None => seen.push((it.site.kind.as_str(), 1)),
        }
    }
    let parts: Vec<String> = seen.iter().map(|(kind, n)| format!("{n} {}", kinds.get(*kind).map_or(*kind, |k| k.label.as_str()))).collect();
    parts.join(", ")
}

/// The table `format` table prints: the workspace's line, then the `top` crates with the most
/// mechanical findings, then the most findings.
fn table(kinds: &BTreeMap<String, KindRow>, ranked: &[Ranked], top: usize) -> String {
    let mut out = String::new();
    let crates: BTreeSet<&str> = ranked.iter().map(|it| it.site.krate.as_str()).collect();
    let skipped: Vec<&str> = NOT_RUN.iter().map(|(source, _)| *source).collect();
    out.push_str(&format!(
        "{SCRIPT}: {} findings in {} crates from {} ratchets ({} not run: no `findings` command)\n",
        ranked.len(),
        crates.len(),
        SOURCES.len(),
        skipped.join(", ")
    ));
    let mut workspace: Vec<&Ranked> = ranked.iter().collect();
    // The workspace's line orders kinds by their total, mechanical first.
    let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
    for it in ranked {
        *totals.entry(it.site.kind.as_str()).or_default() += 1;
    }
    workspace.sort_by(|a, b| {
        (b.mechanical, totals[b.site.kind.as_str()], &a.site.kind).cmp(&(a.mechanical, totals[a.site.kind.as_str()], &b.site.kind))
    });
    out.push_str(&format!("workspace: {}\n", phrase(kinds, &workspace)));
    let mut by_crate: BTreeMap<&str, Vec<&Ranked>> = BTreeMap::new();
    for it in ranked {
        by_crate.entry(it.site.krate.as_str()).or_default().push(it);
    }
    let mut order: Vec<(&str, usize, usize)> =
        by_crate.iter().map(|(krate, it)| (*krate, it.iter().filter(|r| r.mechanical).count(), it.len())).collect();
    order.sort_by(|a, b| (b.1, b.2, a.0).cmp(&(a.1, a.2, b.0)));
    for (krate, ..) in order.iter().take(top) {
        out.push_str(&format!("crate {krate}: {}\n", phrase(kinds, &by_crate[krate])));
    }
    if order.len() > top {
        out.push_str(&format!("({} more crates: give `crate`, or `format` json)\n", order.len() - top));
    }
    out
}

/// `source`'s document, run beside this binary.
fn run_source(source: &str, root: &Path) -> Result<Doc, String> {
    let exe = std::env::current_exe().map_err(|e| format!("advise's own path is unknown: {e}"))?;
    let bin = exe.with_file_name(source);
    if !bin.is_file() {
        return Err(format!("{} is not built: build the `scripts` package's binaries (advise runs `{source}` beside itself)", bin.display()));
    }
    let arg = json::json!({"command": "findings", "root": root.to_string_lossy()}).to_string();
    let shown = bin.to_string_lossy().into_owned();
    let ran = run_within(&[shown.as_str(), arg.as_str()], root, LIMIT)?
        .ok_or_else(|| format!("`{source}` did not answer in {} s: a hang is a bug (AGENTS.md rule 69)", LIMIT.as_secs()))?;
    if ran.status != 0 {
        return Err(format!("`{source}` exited {}: {}", ran.status, ran.err().trim()));
    }
    parse_doc(source, &ran.out())
}

fn main_with(it: &Args) {
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let Some(map_text) = read_text(&root.join(STD_MAP)) else { fail(SCRIPT, &format!("{STD_MAP} could not be read")) };
    let map = parse_std_map(&map_text).unwrap_or_else(|e| fail(SCRIPT, &e));
    let mut docs: Vec<Doc> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    fan_out(SOURCES.len(), &SOURCES, |source| run_source(source, &root), |done| match done {
        Ok(doc) => docs.push(doc),
        Err(e) => errors.push(e),
    });
    if !errors.is_empty() {
        fail(SCRIPT, &errors.join("\n"));
    }
    let docs: Vec<Doc> = docs
        .into_iter()
        .map(|doc| if doc.source == "ambient" { refine(doc, &map, |file| read_text(&root.join(file))) } else { doc })
        .collect();
    if let Some(krate) = &it.krate {
        if !docs.iter().any(|d| d.sites.iter().any(|s| &s.krate == krate)) && !root.join("crates").join(krate).is_dir() {
            fail(SCRIPT, &format!("`{krate}` is no crate under crates/"));
        }
    }
    let (kinds, ranked) = rank(&docs, it.krate.as_deref());
    match it.format.as_str() {
        "json" => println!("{}", document(it.krate.as_deref(), &docs, &kinds, &ranked)),
        _ => print!("{}", table(&kinds, &ranked, it.top)),
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    let ambient = r#"{"schema":"blaze-findings/1","source":"ambient","kinds":[{"name":"std","label":"lines naming `std`","rule":"75","instead":"x","mechanical":false}],
        "findings":[{"crate":"a","file":"crates/a/src/lib.rs","line":1,"kind":"std","detail":null},
                    {"crate":"a","file":"crates/a/src/lib.rs","line":2,"kind":"std","detail":null},
                    {"crate":"a","file":"crates/a/src/lib.rs","line":3,"kind":"std","detail":null}]}"#;
    let casts = r#"{"schema":"blaze-findings/1","source":"casts","kinds":[{"name":"cast","label":"bare casts","rule":"76","instead":"blaze_int","mechanical":false}],
        "findings":[{"crate":"a","file":"crates/a/src/lib.rs","line":9,"kind":"cast","detail":null},
                    {"crate":"b","file":"crates/b/src/lib.rs","line":1,"kind":"cast","detail":null},
                    {"crate":"b","file":"crates/b/src/lib.rs","line":2,"kind":"cast","detail":null}]}"#;
    let map = r#"{"rows":[
        {"path":"std::collections::HashMap","written":"std::collections::HashMap","class":"rename","replacement":"blaze_hash::HashMap"},
        {"path":"alloc::string::String","written":"String","class":"add-arg","replacement":"blaze_own::String<A>"},
        {"path":"std::sync::Mutex","written":"std::sync::Mutex","class":"no-form","replacement":"blaze_core::lock"}]}"#;
    let lines = "use std::{collections::HashMap, fmt};\nfn f(s: String) -> MyString { s }\nuse std::io;\n";
    let or_fail = |it: Result<Doc, String>| it.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    let map = parse_std_map(map).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    let ambient = refine(or_fail(parse_doc("ambient", ambient)), &map, |_| Some(lines.to_owned()));
    let found: Vec<(usize, &str)> = ambient.sites.iter().map(|s| (s.line, s.kind.as_str())).collect();
    // Line 1 by the path less its root, line 2 by the whole word `String` (not `MyString`), line 3 by none.
    expect(found == [(1, "ambient.rename"), (2, "ambient.add-arg"), (3, "ambient.std")], &format!("the refined lines: {found:?}"));
    let docs = vec![ambient, or_fail(parse_doc("casts", casts))];
    let (kinds, ranked) = rank(&docs, None);
    let order: Vec<(&str, &str)> = ranked.iter().map(|r| (r.site.kind.as_str(), r.site.krate.as_str())).collect();
    let want = [("ambient.add-arg", "a"), ("ambient.rename", "a"), ("casts.cast", "b"), ("casts.cast", "b"), ("ambient.std", "a"), ("casts.cast", "a")];
    expect(order == want, &format!("mechanical first, then by count, then kind: {order:?}"));
    let shown = table(&kinds, &ranked, 25);
    expect(shown.contains("crate a: 1 std uses that take an allocator argument, 1 mechanical std renames, 1 lines naming `std`, 1 bare casts\n"), &shown);
    expect(shown.contains("crate b: 2 bare casts\n") && shown.find("crate a").is_some_and(|a| shown.find("crate b").is_some_and(|b| a < b)), &shown);
    let doc = document(None, &docs, &kinds, &ranked);
    let first = &doc["findings"][0];
    expect(first["instead"] == "`blaze_own::String<A>`" && first["measured"].is_null() && first["weight"] == (1u64 << 32) + 1, &format!("{first}"));
    expect(document(None, &docs, &kinds, &ranked) == doc, "two runs print equal documents");
    let (_, only_b) = rank(&docs, Some("b"));
    expect(only_b.len() == 2, "one crate's findings");
    expect(parse_doc("x", r#"{"schema":"other"}"#).is_err(), "another schema is refused");
    expect(
        parse_doc("x", r#"{"schema":"blaze-findings/1","kinds":[],"findings":[{"crate":"a","file":"f","line":1,"kind":"k","detail":null}]}"#).is_err(),
        "a kind its document does not name is refused",
    );
    println!("{SCRIPT} selftest: ok");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too.
    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
