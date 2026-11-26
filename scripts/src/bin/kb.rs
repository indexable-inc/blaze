//! kb: code-anchored, git-tracked knowledge base CLI for agents and contributors.
//!
//! Manages three canonical JSONL tables under `agents/kb/`:
//! - `agents/kb/facts.jsonl` — operational, build, test, and codebase facts/gotchas with SHA-256
//!   `path_hashes` staleness anchoring.
//! - `agents/kb/dead_ends.jsonl` — tried-and-reverted experiments ("negative knowledge").
//! - `agents/kb/baselines.jsonl` — measured hardware-floor and Amdahl decompositions.
//!
//!     blaze run -q -p scripts --bin kb -- '{"command":"query","paths":["crates/blaze-cli"],"crates":["blaze-cli"]}'
//!     blaze run -q -p scripts --bin kb -- '{"command":"add","table":"facts","entry":{...}}'
//!     blaze run -q -p scripts --bin kb -- '{"command":"refresh","id":"...","commit":"...","date":"YYYY-MM-DD"}'
//!     blaze run -q -p scripts --bin kb -- '{"command":"check","fail_on_stale":false}'
//!     blaze run -q -p scripts --bin kb -- '{"command":"migrate_annoyances"}'
//!     blaze run -q -p scripts --bin kb -- selftest

#![forbid(unsafe_code)]

const DOC: &str = r#"scripts/src/bin/kb.rs: code-anchored knowledge base CLI over agents/kb/*.jsonl.

    blaze run -q -p scripts --bin kb -- '{"command":"query", ...}'
    blaze run -q -p scripts --bin kb -- '{"command":"add", ...}'
    blaze run -q -p scripts --bin kb -- '{"command":"refresh", ...}'
    blaze run -q -p scripts --bin kb -- '{"command":"check", ...}'
    blaze run -q -p scripts --bin kb -- '{"command":"migrate_annoyances", ...}'
    blaze run -q -p scripts --bin kb -- selftest

Commands (one JSON object, unknown fields refused, Rule 42):

  query
    root           checkout root (default ".")
    paths          list of relative paths to match (exact +100, prefix/parent +60)
    crates         list of crate names to match (+50)
    symbols        list of symbol names to match (+40)
    text           optional case-insensitive keyword query (+20..+35)
    tables         subset of ["facts","dead_ends","baselines"] (default all three)
    include_stale  include facts whose anchored path_hashes changed/missing (default true)
    limit          max hits to return, 1..=1000 (default 15)
    format         "text" or "json" (default "text")

  add
    root           checkout root (default ".")
    table          "facts", "dead_ends", or "baselines"
    entry          typed object matching the table schema (path_hashes auto-computed for facts)
    overwrite      replace an existing entry with the same id (default false)

  refresh
    root           checkout root (default ".")
    id             facts entry id to re-hash against current files on disk
    commit         optional updated verified_commit
    date           optional updated verified_date (YYYY-MM-DD)

  check
    root           checkout root (default ".")
    fail_on_stale  exit non-zero if any anchored path_hash is stale (default false)

  migrate_annoyances
    root           checkout root (default ".")
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, exit, expect, fail,
    input::{self, Fields, Input, InputError},
};
use serde_json::{Map, Number, Value};

const SCRIPT: &str = "scripts/src/bin/kb.rs";
const KB_DIR: &str = "agents/kb";
const LEGACY_ANNOYANCES_DIR: &str = "agents/annoyances";

const ALL_TABLES: [&str; 3] = ["facts", "dead_ends", "baselines"];
const FACT_KINDS: [&str; 3] = ["fact", "gotcha", "howto"];
const QUERY_FORMATS: [&str; 2] = ["text", "json"];
const COMMANDS: [&str; 5] = ["query", "add", "refresh", "check", "migrate_annoyances"];

const DEFAULT_QUERY_LIMIT: i64 = 15;
const MAX_QUERY_LIMIT: i64 = 1000;

const SCORE_EXACT_PATH: u32 = 100;
const SCORE_PREFIX_PATH: u32 = 60;
const SCORE_CRATE: u32 = 50;
const SCORE_SYMBOL: u32 = 40;
const SCORE_TEXT_BASE: u32 = 20;
const SCORE_TEXT_EXTRA_CAP: u32 = 15;

const REPO_PATH_PREFIXES: [&str; 11] = [
    "crates/",
    "scripts/",
    "tools/",
    "docs/",
    "harness/",
    "library/",
    "pins/",
    "site/",
    "agents/",
    "lean/",
    ".github/",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TableKind {
    Facts,
    DeadEnds,
    Baselines,
}

impl TableKind {
    fn from_name(name: &str) -> Option<TableKind> {
        match name {
            "facts" => Some(TableKind::Facts),
            "dead_ends" => Some(TableKind::DeadEnds),
            "baselines" => Some(TableKind::Baselines),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            TableKind::Facts => "facts",
            TableKind::DeadEnds => "dead_ends",
            TableKind::Baselines => "baselines",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            TableKind::Facts => "facts.jsonl",
            TableKind::DeadEnds => "dead_ends.jsonl",
            TableKind::Baselines => "baselines.jsonl",
        }
    }

    fn path_under(self, root: &Path) -> PathBuf {
        root.join(KB_DIR).join(self.file_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FactEntry {
    id: String,
    kind: String,
    crates: Vec<String>,
    paths: Vec<String>,
    symbols: Vec<String>,
    summary: String,
    detail: String,
    verified_commit: String,
    verified_date: String,
    path_hashes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeadEndEntry {
    id: String,
    crates: Vec<String>,
    paths: Vec<String>,
    symbols: Vec<String>,
    tried: String,
    measured_effect: String,
    root_cause: String,
    class: String,
    commit: String,
    date: String,
}

#[derive(Debug, Clone, PartialEq)]
struct BreakdownItem {
    component: String,
    value: f64,
    unit: String,
}

#[derive(Debug, Clone, PartialEq)]
struct BaselineEntry {
    id: String,
    machine: String,
    cores: u64,
    commit: String,
    date: String,
    workload: String,
    wall_ms: f64,
    rss_mb: f64,
    breakdown: Vec<BreakdownItem>,
    floor_note: String,
}

#[derive(Debug, Clone)]
enum TableEntry {
    Fact(FactEntry),
    DeadEnd(DeadEndEntry),
    Baseline(BaselineEntry),
}

impl TableEntry {
    fn table(&self) -> TableKind {
        match self {
            TableEntry::Fact(_) => TableKind::Facts,
            TableEntry::DeadEnd(_) => TableKind::DeadEnds,
            TableEntry::Baseline(_) => TableKind::Baselines,
        }
    }

    fn id(&self) -> &str {
        match self {
            TableEntry::Fact(e) => &e.id,
            TableEntry::DeadEnd(e) => &e.id,
            TableEntry::Baseline(e) => &e.id,
        }
    }
}

#[derive(Debug, Clone)]
struct QueryArgs {
    root: String,
    paths: Vec<String>,
    crates: Vec<String>,
    symbols: Vec<String>,
    text: Option<String>,
    tables: Vec<TableKind>,
    include_stale: bool,
    limit: usize,
    format: QueryFormat,
}

#[derive(Debug, Clone)]
struct AddArgs {
    root: String,
    entry: TableEntry,
    overwrite: bool,
}

#[derive(Debug, Clone)]
struct RefreshArgs {
    root: String,
    id: String,
    commit: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Clone)]
struct CheckArgs {
    root: String,
    fail_on_stale: bool,
}

#[derive(Debug, Clone)]
struct MigrateArgs {
    root: String,
}

#[derive(Debug, Clone)]
enum Command {
    Query(Box<QueryArgs>),
    Add(Box<AddArgs>),
    Refresh(RefreshArgs),
    Check(CheckArgs),
    MigrateAnnoyances(MigrateArgs),
}

// ---------------------------------------------------------------------------
// Input validation helpers
// ---------------------------------------------------------------------------

fn validate_non_empty(value: &str, field: &str) -> Result<(), InputError> {
    if value.trim().is_empty() {
        return Err(InputError(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_id(id: &str, field: &str) -> Result<(), InputError> {
    if id.is_empty() || id.trim() != id || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(InputError(format!(
            "{field} must be a non-empty identifier without whitespace or control characters, got {id:?}"
        )));
    }
    Ok(())
}

fn is_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes[0..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let month = (bytes[5] - b'0') * 10 + (bytes[6] - b'0');
    let day = (bytes[8] - b'0') * 10 + (bytes[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn validate_date(date: &str, field: &str) -> Result<(), InputError> {
    if !is_iso_date(date) {
        return Err(InputError(format!("{field} must be YYYY-MM-DD, got {date:?}")));
    }
    Ok(())
}

fn is_sha256_hex(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn normalize_rel_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("path must not be empty".to_owned());
    }
    if trimmed.contains('\0') {
        return Err(format!("path {raw:?} contains NUL byte"));
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(format!("path {raw:?} must be relative to root, not absolute"));
    }
    let mut cleaned = trimmed;
    while let Some(rest) = cleaned.strip_prefix("./") {
        cleaned = rest;
    }
    cleaned = cleaned.trim_end_matches('/');
    if cleaned.is_empty() {
        return Err(format!("path {raw:?} resolves to root itself"));
    }
    for seg in cleaned.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
            return Err(format!("path {raw:?} has invalid segment {seg:?}"));
        }
    }
    Ok(cleaned.to_owned())
}

fn normalize_string_list(items: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for item in items {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_owned());
        }
    }
    set.into_iter().collect()
}

fn normalize_path_list(items: &[String], field: &str) -> Result<Vec<String>, InputError> {
    let mut set = BTreeSet::new();
    for (idx, item) in items.iter().enumerate() {
        let norm = normalize_rel_path(item)
            .map_err(|msg| InputError(format!("{field}[{idx}]: {msg}")))?;
        set.insert(norm);
    }
    Ok(set.into_iter().collect())
}

fn parse_path_hashes_map(
    f: &Fields<'_>,
    name: &str,
    required: bool,
) -> Result<BTreeMap<String, String>, InputError> {
    let Some(val) = f.get(name) else {
        return if required {
            Err(InputError(format!("field {} is required", f.at(name))))
        } else {
            Ok(BTreeMap::new())
        };
    };
    let Value::Object(obj) = val else {
        return Err(InputError(format!("{} must be a JSON object mapping path to sha256 hex", f.at(name))));
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        let norm_key = normalize_rel_path(k)
            .map_err(|msg| InputError(format!("{}.{k}: {msg}", f.at(name))))?;
        let Value::String(hex) = v else {
            return Err(InputError(format!("{}.{k} must be a sha256 hex string", f.at(name))));
        };
        if !is_sha256_hex(hex) {
            return Err(InputError(format!(
                "{}.{k} must be 64 lowercase hex characters, got {hex:?}",
                f.at(name)
            )));
        }
        out.insert(norm_key, hex.clone());
    }
    Ok(out)
}

fn parse_finite_f64(f: &Fields<'_>, name: &str, non_negative: bool) -> Result<f64, InputError> {
    let Some(val) = f.get(name) else {
        return Err(InputError(format!("field {} is required", f.at(name))));
    };
    let Value::Number(num) = val else {
        return Err(InputError(format!("{} must be a finite number", f.at(name))));
    };
    let Some(v) = num.as_f64() else {
        return Err(InputError(format!("{} is out of f64 range", f.at(name))));
    };
    if !v.is_finite() {
        return Err(InputError(format!("{} must be finite", f.at(name))));
    }
    if non_negative && v < 0.0 {
        return Err(InputError(format!("{} must be >= 0, got {v}", f.at(name))));
    }
    Ok(v)
}

fn parse_positive_u64(f: &Fields<'_>, name: &str) -> Result<u64, InputError> {
    let Some(val) = f.get(name) else {
        return Err(InputError(format!("field {} is required", f.at(name))));
    };
    let Value::Number(num) = val else {
        return Err(InputError(format!("{} must be a positive integer", f.at(name))));
    };
    let Some(v) = num.as_u64() else {
        return Err(InputError(format!("{} must be a positive integer", f.at(name))));
    };
    if v == 0 {
        return Err(InputError(format!("{} must be >= 1, got 0", f.at(name))));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Input parsing for entries and commands (Rule 42)
// ---------------------------------------------------------------------------

struct FactAddInput(FactEntry);
struct FactDiskInput(FactEntry);
struct DeadEndAddInput(DeadEndEntry);
struct DeadEndDiskInput(DeadEndEntry);
struct BaselineAddInput(BaselineEntry);
struct BaselineDiskInput(BaselineEntry);

const FACT_FIELDS: &[&str] = &[
    "id",
    "kind",
    "crates",
    "paths",
    "symbols",
    "summary",
    "detail",
    "verified_commit",
    "verified_date",
    "path_hashes",
];

const DEAD_END_FIELDS: &[&str] = &[
    "id",
    "crates",
    "paths",
    "symbols",
    "tried",
    "measured_effect",
    "root_cause",
    "class",
    "commit",
    "date",
];

const BASELINE_FIELDS: &[&str] = &[
    "id",
    "machine",
    "cores",
    "commit",
    "date",
    "workload",
    "wall_ms",
    "rss_mb",
    "breakdown",
    "floor_note",
];

impl Input for BreakdownItem {
    const FIELDS: &'static [&'static str] = &["component", "value", "unit"];

    fn parse(f: &Fields<'_>) -> Result<BreakdownItem, InputError> {
        let component = f.str("component", None)?;
        validate_non_empty(&component, &f.at("component"))?;
        let value = parse_finite_f64(f, "value", false)?;
        let unit = f.str("unit", None)?;
        validate_non_empty(&unit, &f.at("unit"))?;
        Ok(BreakdownItem { component, value, unit })
    }
}

fn parse_fact_entry(f: &Fields<'_>, strict_disk: bool) -> Result<FactEntry, InputError> {
    let id = f.str("id", None)?;
    validate_id(&id, &f.at("id"))?;
    let kind = f.one_of("kind", &FACT_KINDS, None)?;
    let crates_raw = if strict_disk { f.strs_required("crates")? } else { f.strs("crates")? };
    let paths_raw = if strict_disk { f.strs_required("paths")? } else { f.strs("paths")? };
    let symbols_raw = if strict_disk { f.strs_required("symbols")? } else { f.strs("symbols")? };
    let summary = f.str("summary", None)?;
    validate_non_empty(&summary, &f.at("summary"))?;
    let detail = f.str("detail", None)?;
    let verified_commit = f.str("verified_commit", None)?;
    validate_non_empty(&verified_commit, &f.at("verified_commit"))?;
    let verified_date = f.str("verified_date", None)?;
    validate_date(&verified_date, &f.at("verified_date"))?;
    let path_hashes = parse_path_hashes_map(f, "path_hashes", strict_disk)?;

    Ok(FactEntry {
        id,
        kind,
        crates: normalize_string_list(&crates_raw),
        paths: normalize_path_list(&paths_raw, &f.at("paths"))?,
        symbols: normalize_string_list(&symbols_raw),
        summary,
        detail,
        verified_commit,
        verified_date,
        path_hashes,
    })
}

impl Input for FactAddInput {
    const FIELDS: &'static [&'static str] = FACT_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<FactAddInput, InputError> {
        parse_fact_entry(f, false).map(FactAddInput)
    }
}

impl Input for FactDiskInput {
    const FIELDS: &'static [&'static str] = FACT_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<FactDiskInput, InputError> {
        parse_fact_entry(f, true).map(FactDiskInput)
    }
}

fn parse_dead_end_entry(f: &Fields<'_>, strict_disk: bool) -> Result<DeadEndEntry, InputError> {
    let id = f.str("id", None)?;
    validate_id(&id, &f.at("id"))?;
    let crates_raw = if strict_disk { f.strs_required("crates")? } else { f.strs("crates")? };
    let paths_raw = if strict_disk { f.strs_required("paths")? } else { f.strs("paths")? };
    let symbols_raw = if strict_disk { f.strs_required("symbols")? } else { f.strs("symbols")? };
    let tried = f.str("tried", None)?;
    validate_non_empty(&tried, &f.at("tried"))?;
    let measured_effect = f.str("measured_effect", None)?;
    let root_cause = f.str("root_cause", None)?;
    let class = f.str("class", None)?;
    let commit = f.str("commit", None)?;
    validate_non_empty(&commit, &f.at("commit"))?;
    let date = f.str("date", None)?;
    validate_date(&date, &f.at("date"))?;

    Ok(DeadEndEntry {
        id,
        crates: normalize_string_list(&crates_raw),
        paths: normalize_path_list(&paths_raw, &f.at("paths"))?,
        symbols: normalize_string_list(&symbols_raw),
        tried,
        measured_effect,
        root_cause,
        class,
        commit,
        date,
    })
}

impl Input for DeadEndAddInput {
    const FIELDS: &'static [&'static str] = DEAD_END_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<DeadEndAddInput, InputError> {
        parse_dead_end_entry(f, false).map(DeadEndAddInput)
    }
}

impl Input for DeadEndDiskInput {
    const FIELDS: &'static [&'static str] = DEAD_END_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<DeadEndDiskInput, InputError> {
        parse_dead_end_entry(f, true).map(DeadEndDiskInput)
    }
}

fn parse_baseline_entry(f: &Fields<'_>, strict_disk: bool) -> Result<BaselineEntry, InputError> {
    let id = f.str("id", None)?;
    validate_id(&id, &f.at("id"))?;
    let machine = f.str("machine", None)?;
    validate_non_empty(&machine, &f.at("machine"))?;
    let cores = parse_positive_u64(f, "cores")?;
    let commit = f.str("commit", None)?;
    validate_non_empty(&commit, &f.at("commit"))?;
    let date = f.str("date", None)?;
    validate_date(&date, &f.at("date"))?;
    let workload = f.str("workload", None)?;
    validate_non_empty(&workload, &f.at("workload"))?;
    let wall_ms = parse_finite_f64(f, "wall_ms", true)?;
    let rss_mb = parse_finite_f64(f, "rss_mb", true)?;
    let breakdown = f.objects::<BreakdownItem>("breakdown", strict_disk)?;
    let floor_note = f.str("floor_note", None)?;

    Ok(BaselineEntry {
        id,
        machine,
        cores,
        commit,
        date,
        workload,
        wall_ms,
        rss_mb,
        breakdown,
        floor_note,
    })
}

impl Input for BaselineAddInput {
    const FIELDS: &'static [&'static str] = BASELINE_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<BaselineAddInput, InputError> {
        parse_baseline_entry(f, false).map(BaselineAddInput)
    }
}

impl Input for BaselineDiskInput {
    const FIELDS: &'static [&'static str] = BASELINE_FIELDS;

    fn parse(f: &Fields<'_>) -> Result<BaselineDiskInput, InputError> {
        parse_baseline_entry(f, true).map(BaselineDiskInput)
    }
}

impl Input for QueryArgs {
    const FIELDS: &'static [&'static str] = &[
        "command",
        "root",
        "paths",
        "crates",
        "symbols",
        "text",
        "tables",
        "include_stale",
        "limit",
        "format",
    ];

    fn parse(f: &Fields<'_>) -> Result<QueryArgs, InputError> {
        let _ = f.one_of("command", &["query"], None)?;
        let root = f.str("root", Some("."))?;
        validate_non_empty(&root, &f.at("root"))?;
        let paths = f.strs("paths")?;
        let crates = f.strs("crates")?;
        let symbols = f.strs("symbols")?;
        let text = f.str_or_null("text")?;
        let table_names = f.ones_of("tables", &ALL_TABLES, &ALL_TABLES)?;
        if table_names.is_empty() {
            return Err(InputError(format!("{} must name at least one table", f.at("tables"))));
        }
        let mut tables = Vec::with_capacity(table_names.len());
        for name in &table_names {
            let Some(kind) = TableKind::from_name(name) else {
                return Err(InputError(format!("unknown table {name:?}")));
            };
            if !tables.contains(&kind) {
                tables.push(kind);
            }
        }
        let include_stale = f.bool("include_stale", Some(true))?;
        let limit_raw = f.int("limit", Some(DEFAULT_QUERY_LIMIT))?;
        if !(1..=MAX_QUERY_LIMIT).contains(&limit_raw) {
            return Err(InputError(format!(
                "{} must be in 1..={MAX_QUERY_LIMIT}, not {limit_raw}",
                f.at("limit")
            )));
        }
        let limit = usize::try_from(limit_raw)
            .map_err(|_| InputError(format!("{} out of range: {limit_raw}", f.at("limit"))))?;
        let format_str = f.one_of("format", &QUERY_FORMATS, Some("text"))?;
        let format = match format_str.as_str() {
            "text" => QueryFormat::Text,
            "json" => QueryFormat::Json,
            other => return Err(InputError(format!("unknown format {other:?}"))),
        };
        Ok(QueryArgs { root, paths, crates, symbols, text, tables, include_stale, limit, format })
    }
}

impl Input for AddArgs {
    const FIELDS: &'static [&'static str] = &["command", "root", "table", "entry", "overwrite"];

    fn parse(f: &Fields<'_>) -> Result<AddArgs, InputError> {
        let _ = f.one_of("command", &["add"], None)?;
        let root = f.str("root", Some("."))?;
        validate_non_empty(&root, &f.at("root"))?;
        let table_str = f.one_of("table", &ALL_TABLES, None)?;
        let Some(table) = TableKind::from_name(&table_str) else {
            return Err(InputError(format!("unknown table {table_str:?}")));
        };
        let Some(entry_val) = f.get("entry") else {
            return Err(InputError(format!("field {} is required", f.at("entry"))));
        };
        let entry = match table {
            TableKind::Facts => {
                TableEntry::Fact(input::of::<FactAddInput>(entry_val, &f.at("entry"))?.0)
            }
            TableKind::DeadEnds => {
                TableEntry::DeadEnd(input::of::<DeadEndAddInput>(entry_val, &f.at("entry"))?.0)
            }
            TableKind::Baselines => {
                TableEntry::Baseline(input::of::<BaselineAddInput>(entry_val, &f.at("entry"))?.0)
            }
        };
        let overwrite = f.bool("overwrite", Some(false))?;
        Ok(AddArgs { root, entry, overwrite })
    }
}

impl Input for RefreshArgs {
    const FIELDS: &'static [&'static str] = &["command", "root", "id", "commit", "date"];

    fn parse(f: &Fields<'_>) -> Result<RefreshArgs, InputError> {
        let _ = f.one_of("command", &["refresh"], None)?;
        let root = f.str("root", Some("."))?;
        validate_non_empty(&root, &f.at("root"))?;
        let id = f.str("id", None)?;
        validate_id(&id, &f.at("id"))?;
        let commit = f.str_or_null("commit")?;
        if let Some(ref c) = commit {
            validate_non_empty(c, &f.at("commit"))?;
        }
        let date = f.str_or_null("date")?;
        if let Some(ref d) = date {
            validate_date(d, &f.at("date"))?;
        }
        Ok(RefreshArgs { root, id, commit, date })
    }
}

impl Input for CheckArgs {
    const FIELDS: &'static [&'static str] = &["command", "root", "fail_on_stale"];

    fn parse(f: &Fields<'_>) -> Result<CheckArgs, InputError> {
        let _ = f.one_of("command", &["check"], None)?;
        let root = f.str("root", Some("."))?;
        validate_non_empty(&root, &f.at("root"))?;
        let fail_on_stale = f.bool("fail_on_stale", Some(false))?;
        Ok(CheckArgs { root, fail_on_stale })
    }
}

impl Input for MigrateArgs {
    const FIELDS: &'static [&'static str] = &["command", "root"];

    fn parse(f: &Fields<'_>) -> Result<MigrateArgs, InputError> {
        let _ = f.one_of("command", &["migrate_annoyances"], None)?;
        let root = f.str("root", Some("."))?;
        validate_non_empty(&root, &f.at("root"))?;
        Ok(MigrateArgs { root })
    }
}

fn parse_command_value(value: &Value) -> Result<Command, InputError> {
    let Value::Object(obj) = value else {
        return Err(InputError(format!("the input is a JSON object, not {value}")));
    };
    let Some(cmd_val) = obj.get("command") else {
        return Err(InputError("field command is required".to_owned()));
    };
    let Value::String(cmd) = cmd_val else {
        return Err(InputError(format!("command must be a string, not {cmd_val}")));
    };
    match cmd.as_str() {
        "query" => input::of::<QueryArgs>(value, "").map(|a| Command::Query(Box::new(a))),
        "add" => input::of::<AddArgs>(value, "").map(|a| Command::Add(Box::new(a))),
        "refresh" => input::of::<RefreshArgs>(value, "").map(Command::Refresh),
        "check" => input::of::<CheckArgs>(value, "").map(Command::Check),
        "migrate_annoyances" => input::of::<MigrateArgs>(value, "").map(Command::MigrateAnnoyances),
        _ => Err(InputError(format!(
            "command is one of {}, not {cmd:?}",
            COMMANDS.join(", ")
        ))),
    }
}

// ---------------------------------------------------------------------------
// Canonical JSON serialization (sorted keys, 1-line JSON, Rule 25)
// ---------------------------------------------------------------------------

fn string_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
}

fn sorted_object(fields: BTreeMap<&str, Value>) -> Value {
    let mut map = Map::new();
    for (k, v) in fields {
        map.insert(k.to_owned(), v);
    }
    Value::Object(map)
}

fn finite_json_number(value: f64, label: &str) -> Result<Value, String> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| format!("{label} is not representable in JSON: {value}"))
}

fn fact_to_value(entry: &FactEntry) -> Value {
    let mut hashes = Map::new();
    for (k, v) in &entry.path_hashes {
        hashes.insert(k.clone(), Value::String(v.clone()));
    }
    let mut fields = BTreeMap::new();
    fields.insert("crates", string_array(&entry.crates));
    fields.insert("detail", Value::String(entry.detail.clone()));
    fields.insert("id", Value::String(entry.id.clone()));
    fields.insert("kind", Value::String(entry.kind.clone()));
    fields.insert("path_hashes", Value::Object(hashes));
    fields.insert("paths", string_array(&entry.paths));
    fields.insert("summary", Value::String(entry.summary.clone()));
    fields.insert("symbols", string_array(&entry.symbols));
    fields.insert("verified_commit", Value::String(entry.verified_commit.clone()));
    fields.insert("verified_date", Value::String(entry.verified_date.clone()));
    sorted_object(fields)
}

fn dead_end_to_value(entry: &DeadEndEntry) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("class", Value::String(entry.class.clone()));
    fields.insert("commit", Value::String(entry.commit.clone()));
    fields.insert("crates", string_array(&entry.crates));
    fields.insert("date", Value::String(entry.date.clone()));
    fields.insert("id", Value::String(entry.id.clone()));
    fields.insert("measured_effect", Value::String(entry.measured_effect.clone()));
    fields.insert("paths", string_array(&entry.paths));
    fields.insert("root_cause", Value::String(entry.root_cause.clone()));
    fields.insert("symbols", string_array(&entry.symbols));
    fields.insert("tried", Value::String(entry.tried.clone()));
    sorted_object(fields)
}

fn baseline_to_value(entry: &BaselineEntry) -> Result<Value, String> {
    let mut breakdown = Vec::with_capacity(entry.breakdown.len());
    for item in &entry.breakdown {
        let mut item_fields = BTreeMap::new();
        item_fields.insert("component", Value::String(item.component.clone()));
        item_fields.insert("unit", Value::String(item.unit.clone()));
        item_fields.insert("value", finite_json_number(item.value, "breakdown.value")?);
        breakdown.push(sorted_object(item_fields));
    }
    let mut fields = BTreeMap::new();
    fields.insert("breakdown", Value::Array(breakdown));
    fields.insert("commit", Value::String(entry.commit.clone()));
    fields.insert("cores", Value::Number(Number::from(entry.cores)));
    fields.insert("date", Value::String(entry.date.clone()));
    fields.insert("floor_note", Value::String(entry.floor_note.clone()));
    fields.insert("id", Value::String(entry.id.clone()));
    fields.insert("machine", Value::String(entry.machine.clone()));
    fields.insert("rss_mb", finite_json_number(entry.rss_mb, "rss_mb")?);
    fields.insert("wall_ms", finite_json_number(entry.wall_ms, "wall_ms")?);
    fields.insert("workload", Value::String(entry.workload.clone()));
    Ok(sorted_object(fields))
}

fn entry_to_value(entry: &TableEntry) -> Result<Value, String> {
    match entry {
        TableEntry::Fact(e) => Ok(fact_to_value(e)),
        TableEntry::DeadEnd(e) => Ok(dead_end_to_value(e)),
        TableEntry::Baseline(e) => baseline_to_value(e),
    }
}

fn encode_jsonl_lines(values: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    for val in values {
        let line = serde_json::to_string(val)
            .map_err(|e| format!("failed to serialize JSONL line: {e}"))?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SHA-256 content anchoring & staleness verification
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = blaze_sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DiskHash {
    Missing,
    Hash(String),
}

fn read_disk_hash_cached(
    root: &Path,
    rel_path: &str,
    cache: &mut BTreeMap<String, DiskHash>,
) -> DiskHash {
    if let Some(hit) = cache.get(rel_path) {
        return hit.clone();
    }
    let full = root.join(rel_path);
    let state = match std::fs::metadata(&full) {
        Ok(meta) if meta.is_file() => match std::fs::read(&full) {
            Ok(bytes) => DiskHash::Hash(sha256_hex(&bytes)),
            Err(_) => DiskHash::Missing,
        },
        _ => DiskHash::Missing,
    };
    cache.insert(rel_path.to_owned(), state.clone());
    state
}

fn compute_path_hashes(
    root: &Path,
    paths: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let mut hashes = BTreeMap::new();
    for rel in paths {
        let norm = normalize_rel_path(rel)?;
        let full = root.join(&norm);
        let meta = std::fs::metadata(&full).map_err(|e| {
            format!(
                "anchored path '{norm}' does not exist under '{}': {e}",
                root.display()
            )
        })?;
        if meta.is_file() {
            let bytes = std::fs::read(&full)
                .map_err(|e| format!("failed to read anchored file '{norm}': {e}"))?;
            hashes.insert(norm, sha256_hex(&bytes));
        } else if meta.is_dir() {
            // Directory anchors verify existence without hashing directory entries.
        } else {
            return Err(format!(
                "anchored path '{norm}' under '{}' is neither a regular file nor a directory",
                root.display()
            ));
        }
    }
    Ok(hashes)
}

fn fact_staleness_tags(
    root: &Path,
    fact: &FactEntry,
    cache: &mut BTreeMap<String, DiskHash>,
) -> Vec<String> {
    let mut tags = Vec::new();
    for (rel_path, expected) in &fact.path_hashes {
        match read_disk_hash_cached(root, rel_path, cache) {
            DiskHash::Missing => {
                tags.push(format!("[STALE: missing {rel_path}]"));
            }
            DiskHash::Hash(actual) if actual != *expected => {
                tags.push(format!(
                    "[STALE: changed {rel_path} since {}]",
                    fact.verified_commit
                ));
            }
            DiskHash::Hash(_) => {}
        }
    }
    tags
}

// ---------------------------------------------------------------------------
// JSONL table I/O (atomic writes, strict validation)
// ---------------------------------------------------------------------------

fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create directory {}: {e}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid target path: {}", path.display()))?;
    let tmp = path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
    if let Err(err) = std::fs::write(&tmp, content.as_bytes()) {
        if std::fs::remove_file(&tmp).is_err() {}
        return Err(format!("failed to write {}: {err}", tmp.display()));
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        if std::fs::remove_file(&tmp).is_err() {}
        return Err(format!("failed to replace {}: {err}", path.display()));
    }
    Ok(())
}

struct LoadedTable<T> {
    entries: Vec<T>,
    errors: Vec<String>,
    existed: bool,
}

fn load_jsonl_table<T>(
    path: &Path,
    table_name: &str,
    parse_entry: impl Fn(&Value) -> Result<T, InputError>,
    id_of: impl Fn(&T) -> &str,
) -> LoadedTable<T> {
    if !path.exists() {
        return LoadedTable { entries: Vec::new(), errors: Vec::new(), existed: false };
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            return LoadedTable {
                entries: Vec::new(),
                errors: vec![format!("{table_name}: failed to read {}: {err}", path.display())],
                existed: true,
            };
        }
    };

    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let mut seen_ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut prev_id: Option<String> = None;

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            errors.push(format!("{table_name}:{line_no}: empty line is not allowed in JSONL"));
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                errors.push(format!("{table_name}:{line_no}: invalid JSON: {err}"));
                continue;
            }
        };
        let entry = match parse_entry(&value) {
            Ok(e) => e,
            Err(InputError(msg)) => {
                errors.push(format!("{table_name}:{line_no}: schema error: {msg}"));
                continue;
            }
        };
        let id = id_of(&entry).to_owned();
        if let Some(first_line) = seen_ids.get(&id) {
            errors.push(format!(
                "{table_name}:{line_no}: duplicate id {id:?} (first seen at line {first_line})"
            ));
        } else {
            seen_ids.insert(id.clone(), line_no);
        }
        if let Some(ref prev) = prev_id
            && id.as_str() < prev.as_str()
        {
            errors.push(format!(
                "{table_name}:{line_no}: unsorted id {id:?} appears after {prev:?}"
            ));
        }
        prev_id = Some(id);
        entries.push(entry);
    }

    LoadedTable { entries, errors, existed: true }
}

fn load_facts(root: &Path) -> LoadedTable<FactEntry> {
    load_jsonl_table(
        &TableKind::Facts.path_under(root),
        "facts.jsonl",
        |v| input::of::<FactDiskInput>(v, "").map(|it| it.0),
        |e| &e.id,
    )
}

fn load_dead_ends(root: &Path) -> LoadedTable<DeadEndEntry> {
    load_jsonl_table(
        &TableKind::DeadEnds.path_under(root),
        "dead_ends.jsonl",
        |v| input::of::<DeadEndDiskInput>(v, "").map(|it| it.0),
        |e| &e.id,
    )
}

fn load_baselines(root: &Path) -> LoadedTable<BaselineEntry> {
    load_jsonl_table(
        &TableKind::Baselines.path_under(root),
        "baselines.jsonl",
        |v| input::of::<BaselineDiskInput>(v, "").map(|it| it.0),
        |e| &e.id,
    )
}

fn save_facts(root: &Path, entries: &mut [FactEntry]) -> Result<(), String> {
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    let values: Vec<Value> = entries.iter().map(fact_to_value).collect();
    let content = encode_jsonl_lines(&values)?;
    write_atomic(&TableKind::Facts.path_under(root), &content)
}

fn save_dead_ends(root: &Path, entries: &mut [DeadEndEntry]) -> Result<(), String> {
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    let values: Vec<Value> = entries.iter().map(dead_end_to_value).collect();
    let content = encode_jsonl_lines(&values)?;
    write_atomic(&TableKind::DeadEnds.path_under(root), &content)
}

fn save_baselines(root: &Path, entries: &mut [BaselineEntry]) -> Result<(), String> {
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    let mut values = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        values.push(baseline_to_value(entry)?);
    }
    let content = encode_jsonl_lines(&values)?;
    write_atomic(&TableKind::Baselines.path_under(root), &content)
}

// ---------------------------------------------------------------------------
// Query engine (ranking + staleness verification + rendering)
// ---------------------------------------------------------------------------

fn normalize_query_path(raw: &str) -> String {
    let mut s = raw.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.trim_end_matches('/').to_owned()
}

fn is_path_prefix(prefix: &str, full: &str) -> bool {
    if prefix.is_empty() || full.len() <= prefix.len() {
        return false;
    }
    full.starts_with(prefix) && full.as_bytes()[prefix.len()] == b'/'
}

fn normalize_crate_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('_', "-")
}

fn symbol_matches(query_sym_lower: &str, entry_sym: &str) -> bool {
    let entry_lower = entry_sym.to_ascii_lowercase();
    if entry_lower == query_sym_lower {
        return true;
    }
    let suffix = format!("::{query_sym_lower}");
    if entry_lower.ends_with(&suffix) {
        return true;
    }
    let reverse_suffix = format!("::{entry_lower}");
    if query_sym_lower.ends_with(&reverse_suffix) {
        return true;
    }
    entry_lower.contains(query_sym_lower)
}

fn tokenize_query_text(text: Option<&str>) -> Vec<String> {
    let Some(raw) = text else { return Vec::new() };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for tok in raw.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if !lower.is_empty() && seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

fn score_path_list(query_paths: &[String], entry_paths: &[String]) -> u32 {
    let mut total = 0u32;
    for qp in query_paths {
        let mut best = 0u32;
        for ep in entry_paths {
            if qp == ep {
                best = best.max(SCORE_EXACT_PATH);
            } else if is_path_prefix(qp, ep) || is_path_prefix(ep, qp) {
                best = best.max(SCORE_PREFIX_PATH);
            }
        }
        total = total.saturating_add(best);
    }
    total
}

fn score_crate_list(query_crates: &[String], entry_crates: &[String]) -> u32 {
    let mut total = 0u32;
    for qc in query_crates {
        if entry_crates.iter().any(|ec| normalize_crate_key(ec) == *qc) {
            total = total.saturating_add(SCORE_CRATE);
        }
    }
    total
}

fn score_symbol_list(query_symbols: &[String], entry_symbols: &[String]) -> u32 {
    let mut total = 0u32;
    for qs in query_symbols {
        if entry_symbols.iter().any(|es| symbol_matches(qs, es)) {
            total = total.saturating_add(SCORE_SYMBOL);
        }
    }
    total
}

fn score_text_tokens(query_tokens: &[String], haystack_lower: &str) -> u32 {
    if query_tokens.is_empty() {
        return 0;
    }
    let mut matched = 0u32;
    for tok in query_tokens {
        if haystack_lower.contains(tok.as_str()) {
            matched = matched.saturating_add(1);
        }
    }
    if matched == 0 {
        0
    } else {
        SCORE_TEXT_BASE.saturating_add(matched.saturating_sub(1).min(SCORE_TEXT_EXTRA_CAP))
    }
}

fn entry_haystack_lower(entry: &TableEntry) -> String {
    let mut buf = String::new();
    match entry {
        TableEntry::Fact(f) => {
            buf.push_str(&f.id);
            buf.push(' ');
            buf.push_str(&f.kind);
            buf.push(' ');
            buf.push_str(&f.summary);
            buf.push(' ');
            buf.push_str(&f.detail);
            for item in f.crates.iter().chain(f.paths.iter()).chain(f.symbols.iter()) {
                buf.push(' ');
                buf.push_str(item);
            }
        }
        TableEntry::DeadEnd(d) => {
            buf.push_str(&d.id);
            buf.push(' ');
            buf.push_str(&d.tried);
            buf.push(' ');
            buf.push_str(&d.measured_effect);
            buf.push(' ');
            buf.push_str(&d.root_cause);
            buf.push(' ');
            buf.push_str(&d.class);
            for item in d.crates.iter().chain(d.paths.iter()).chain(d.symbols.iter()) {
                buf.push(' ');
                buf.push_str(item);
            }
        }
        TableEntry::Baseline(b) => {
            buf.push_str(&b.id);
            buf.push(' ');
            buf.push_str(&b.machine);
            buf.push(' ');
            buf.push_str(&b.workload);
            buf.push(' ');
            buf.push_str(&b.floor_note);
            buf.push(' ');
            buf.push_str(&b.commit);
            for item in &b.breakdown {
                buf.push(' ');
                buf.push_str(&item.component);
            }
        }
    }
    buf.to_ascii_lowercase()
}

fn score_entry(
    entry: &TableEntry,
    query_paths: &[String],
    query_crates: &[String],
    query_symbols: &[String],
    query_tokens: &[String],
) -> u32 {
    let mut score = 0u32;
    match entry {
        TableEntry::Fact(f) => {
            score = score.saturating_add(score_path_list(query_paths, &f.paths));
            score = score.saturating_add(score_crate_list(query_crates, &f.crates));
            score = score.saturating_add(score_symbol_list(query_symbols, &f.symbols));
        }
        TableEntry::DeadEnd(d) => {
            score = score.saturating_add(score_path_list(query_paths, &d.paths));
            score = score.saturating_add(score_crate_list(query_crates, &d.crates));
            score = score.saturating_add(score_symbol_list(query_symbols, &d.symbols));
        }
        TableEntry::Baseline(b) => {
            let workload_norm = normalize_crate_key(&b.workload);
            for qc in query_crates {
                if workload_norm == *qc {
                    score = score.saturating_add(SCORE_CRATE);
                }
            }
        }
    }
    if !query_tokens.is_empty() {
        let haystack = entry_haystack_lower(entry);
        score = score.saturating_add(score_text_tokens(query_tokens, &haystack));
    }
    score
}

#[derive(Debug, Clone)]
struct QueryHit {
    score: u32,
    stale: Vec<String>,
    entry: TableEntry,
}

#[derive(Debug, Clone)]
struct QueryResult {
    total_scanned: usize,
    total_matched: usize,
    hits: Vec<QueryHit>,
}

fn run_query(args: &QueryArgs) -> Result<QueryResult, String> {
    let root = Path::new(&args.root);
    let mut candidates: Vec<TableEntry> = Vec::new();
    let mut load_errors: Vec<String> = Vec::new();

    for &table in &args.tables {
        match table {
            TableKind::Facts => {
                let loaded = load_facts(root);
                load_errors.extend(loaded.errors);
                candidates.extend(loaded.entries.into_iter().map(TableEntry::Fact));
            }
            TableKind::DeadEnds => {
                let loaded = load_dead_ends(root);
                load_errors.extend(loaded.errors);
                candidates.extend(loaded.entries.into_iter().map(TableEntry::DeadEnd));
            }
            TableKind::Baselines => {
                let loaded = load_baselines(root);
                load_errors.extend(loaded.errors);
                candidates.extend(loaded.entries.into_iter().map(TableEntry::Baseline));
            }
        }
    }
    if !load_errors.is_empty() {
        return Err(format!("corrupt KB table(s):\n  {}", load_errors.join("\n  ")));
    }

    let total_scanned = candidates.len();
    let query_paths: Vec<String> = args
        .paths
        .iter()
        .map(|p| normalize_query_path(p))
        .filter(|p| !p.is_empty())
        .collect();
    let query_crates: Vec<String> = args
        .crates
        .iter()
        .map(|c| normalize_crate_key(c))
        .filter(|c| !c.is_empty())
        .collect();
    let query_symbols: Vec<String> = args
        .symbols
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let query_tokens = tokenize_query_text(args.text.as_deref());

    let has_filters = !query_paths.is_empty()
        || !query_crates.is_empty()
        || !query_symbols.is_empty()
        || !query_tokens.is_empty();

    let mut scored: Vec<(u32, TableEntry)> = Vec::new();
    for entry in candidates {
        let score = if has_filters {
            score_entry(&entry, &query_paths, &query_crates, &query_symbols, &query_tokens)
        } else {
            0
        };
        if !has_filters || score > 0 {
            scored.push((score, entry));
        }
    }

    scored.sort_by(|(sa, ea), (sb, eb)| {
        sb.cmp(sa)
            .then_with(|| ea.table().cmp(&eb.table()))
            .then_with(|| ea.id().cmp(eb.id()))
    });

    let mut hash_cache: BTreeMap<String, DiskHash> = BTreeMap::new();
    let mut filtered_hits: Vec<QueryHit> = Vec::new();
    for (score, entry) in scored {
        let stale = match &entry {
            TableEntry::Fact(f) => fact_staleness_tags(root, f, &mut hash_cache),
            TableEntry::DeadEnd(_) | TableEntry::Baseline(_) => Vec::new(),
        };
        if !args.include_stale && !stale.is_empty() {
            continue;
        }
        filtered_hits.push(QueryHit { score, stale, entry });
    }

    let total_matched = filtered_hits.len();
    filtered_hits.truncate(args.limit);

    Ok(QueryResult { total_scanned, total_matched, hits: filtered_hits })
}

fn push_fmt_line(out: &mut String, args: std::fmt::Arguments<'_>) {
    if out.write_fmt(args).is_ok() {
        out.push('\n');
    }
}

fn format_meta_line(crates: &[String], paths: &[String], symbols: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if !crates.is_empty() {
        parts.push(format!("crates: {}", crates.join(", ")));
    }
    if !paths.is_empty() {
        parts.push(format!("paths: {}", paths.join(", ")));
    }
    if !symbols.is_empty() {
        parts.push(format!("symbols: {}", symbols.join(", ")));
    }
    if parts.is_empty() { None } else { Some(parts.join(" | ")) }
}

fn render_query_text(result: &QueryResult) -> String {
    let mut out = String::new();
    if result.hits.is_empty() {
        push_fmt_line(
            &mut out,
            format_args!("kb: 0 matches (scanned {} entries)", result.total_scanned),
        );
        return out;
    }
    push_fmt_line(
        &mut out,
        format_args!(
            "kb: showing {} of {} match(es) (scanned {} entries)",
            result.hits.len(),
            result.total_matched,
            result.total_scanned
        ),
    );
    for hit in &result.hits {
        out.push('\n');
        match &hit.entry {
            TableEntry::Fact(f) => {
                if hit.stale.is_empty() {
                    push_fmt_line(
                        &mut out,
                        format_args!("[fact:{}] {} (score={})", f.kind, f.id, hit.score),
                    );
                } else {
                    push_fmt_line(
                        &mut out,
                        format_args!(
                            "[fact:{}] {} (score={}) {}",
                            f.kind,
                            f.id,
                            hit.score,
                            hit.stale.join(" ")
                        ),
                    );
                }
                push_fmt_line(&mut out, format_args!("  summary: {}", f.summary));
                if !f.detail.is_empty() {
                    push_fmt_line(&mut out, format_args!("  detail: {}", f.detail));
                }
                if let Some(meta) = format_meta_line(&f.crates, &f.paths, &f.symbols) {
                    push_fmt_line(&mut out, format_args!("  {meta}"));
                }
                push_fmt_line(
                    &mut out,
                    format_args!("  verified: {} ({})", f.verified_commit, f.verified_date),
                );
            }
            TableEntry::DeadEnd(d) => {
                push_fmt_line(
                    &mut out,
                    format_args!("[dead_end] {} (score={})", d.id, hit.score),
                );
                push_fmt_line(&mut out, format_args!("  tried: {}", d.tried));
                if !d.measured_effect.is_empty() {
                    push_fmt_line(&mut out, format_args!("  effect: {}", d.measured_effect));
                }
                if !d.root_cause.is_empty() {
                    push_fmt_line(&mut out, format_args!("  cause: {}", d.root_cause));
                }
                if !d.class.is_empty() {
                    push_fmt_line(&mut out, format_args!("  class: {}", d.class));
                }
                if let Some(meta) = format_meta_line(&d.crates, &d.paths, &d.symbols) {
                    push_fmt_line(&mut out, format_args!("  {meta}"));
                }
                push_fmt_line(&mut out, format_args!("  commit: {} ({})", d.commit, d.date));
            }
            TableEntry::Baseline(b) => {
                push_fmt_line(
                    &mut out,
                    format_args!("[baseline] {} (score={})", b.id, hit.score),
                );
                push_fmt_line(
                    &mut out,
                    format_args!(
                        "  workload: {} | machine: {} ({} cores) | wall: {:.2} ms | rss: {:.2} MB",
                        b.workload, b.machine, b.cores, b.wall_ms, b.rss_mb
                    ),
                );
                if !b.breakdown.is_empty() {
                    let parts: Vec<String> = b
                        .breakdown
                        .iter()
                        .map(|item| format!("{}={}{}", item.component, item.value, item.unit))
                        .collect();
                    push_fmt_line(&mut out, format_args!("  breakdown: {}", parts.join(", ")));
                }
                if !b.floor_note.is_empty() {
                    push_fmt_line(&mut out, format_args!("  floor_note: {}", b.floor_note));
                }
                push_fmt_line(&mut out, format_args!("  commit: {} ({})", b.commit, b.date));
            }
        }
    }
    out
}

fn render_query_json(result: &QueryResult) -> Result<String, String> {
    let mut hits_json = Vec::with_capacity(result.hits.len());
    for hit in &result.hits {
        let mut hit_fields = BTreeMap::new();
        hit_fields.insert("entry", entry_to_value(&hit.entry)?);
        hit_fields.insert("score", Value::Number(Number::from(hit.score)));
        hit_fields.insert("stale", string_array(&hit.stale));
        hit_fields.insert("table", Value::String(hit.entry.table().as_str().to_owned()));
        hits_json.push(sorted_object(hit_fields));
    }
    let mut top = BTreeMap::new();
    top.insert("hits", Value::Array(hits_json));
    top.insert("total_matched", Value::Number(Number::from(result.total_matched)));
    top.insert("total_scanned", Value::Number(Number::from(result.total_scanned)));
    let mut rendered = serde_json::to_string_pretty(&sorted_object(top))
        .map_err(|e| format!("failed to serialize query JSON: {e}"))?;
    rendered.push('\n');
    Ok(rendered)
}

// ---------------------------------------------------------------------------
// add / refresh / check commands
// ---------------------------------------------------------------------------

fn run_add(args: &AddArgs) -> Result<String, String> {
    let root = Path::new(&args.root);
    match &args.entry {
        TableEntry::Fact(fact_in) => {
            let mut fact = fact_in.clone();
            let computed = compute_path_hashes(root, &fact.paths)?;
            for (k, v) in computed {
                fact.path_hashes.insert(k, v);
            }
            let loaded = load_facts(root);
            if !loaded.errors.is_empty() {
                return Err(format!(
                    "cannot add to corrupt facts.jsonl:\n  {}",
                    loaded.errors.join("\n  ")
                ));
            }
            let mut entries = loaded.entries;
            let fact_id = fact.id.clone();
            let hash_count = fact.path_hashes.len();
            let action = upsert_by_id(&mut entries, fact, args.overwrite, "facts", |e| &e.id)?;
            save_facts(root, &mut entries)?;
            Ok(format!(
                "kb: {action} facts/{fact_id} (path_hashes: {hash_count}, total: {})\n",
                entries.len()
            ))
        }
        TableEntry::DeadEnd(de) => {
            let loaded = load_dead_ends(root);
            if !loaded.errors.is_empty() {
                return Err(format!(
                    "cannot add to corrupt dead_ends.jsonl:\n  {}",
                    loaded.errors.join("\n  ")
                ));
            }
            let mut entries = loaded.entries;
            let action =
                upsert_by_id(&mut entries, de.clone(), args.overwrite, "dead_ends", |e| &e.id)?;
            save_dead_ends(root, &mut entries)?;
            Ok(format!(
                "kb: {action} dead_ends/{} (total: {})\n",
                de.id,
                entries.len()
            ))
        }
        TableEntry::Baseline(bl) => {
            let loaded = load_baselines(root);
            if !loaded.errors.is_empty() {
                return Err(format!(
                    "cannot add to corrupt baselines.jsonl:\n  {}",
                    loaded.errors.join("\n  ")
                ));
            }
            let mut entries = loaded.entries;
            let action =
                upsert_by_id(&mut entries, bl.clone(), args.overwrite, "baselines", |e| &e.id)?;
            save_baselines(root, &mut entries)?;
            Ok(format!(
                "kb: {action} baselines/{} (total: {})\n",
                bl.id,
                entries.len()
            ))
        }
    }
}

fn upsert_by_id<T>(
    entries: &mut Vec<T>,
    item: T,
    overwrite: bool,
    table_name: &str,
    id_of: impl Fn(&T) -> &str,
) -> Result<&'static str, String> {
    let target_id = id_of(&item).to_owned();
    if let Some(pos) = entries.iter().position(|e| id_of(e) == target_id) {
        if !overwrite {
            return Err(format!(
                "duplicate id {target_id:?} already exists in {table_name}.jsonl (pass \"overwrite\": true to replace)"
            ));
        }
        entries[pos] = item;
        Ok("updated")
    } else {
        entries.push(item);
        Ok("added")
    }
}

fn run_refresh(args: &RefreshArgs) -> Result<String, String> {
    let root = Path::new(&args.root);
    let loaded = load_facts(root);
    if !loaded.existed {
        return Err(format!(
            "cannot refresh {:?}: {} does not exist",
            args.id,
            TableKind::Facts.path_under(root).display()
        ));
    }
    if !loaded.errors.is_empty() {
        return Err(format!(
            "cannot refresh corrupt facts.jsonl:\n  {}",
            loaded.errors.join("\n  ")
        ));
    }
    let mut entries = loaded.entries;
    let Some(entry) = entries.iter_mut().find(|e| e.id == args.id) else {
        return Err(format!("facts entry {:?} not found in facts.jsonl", args.id));
    };
    let refreshed_hashes = compute_path_hashes(root, &entry.paths)?;
    let hash_count = refreshed_hashes.len();
    entry.path_hashes = refreshed_hashes;
    if let Some(ref commit) = args.commit {
        entry.verified_commit = commit.clone();
    }
    if let Some(ref date) = args.date {
        entry.verified_date = date.clone();
    }
    let verified_commit = entry.verified_commit.clone();
    let verified_date = entry.verified_date.clone();
    save_facts(root, &mut entries)?;
    Ok(format!(
        "kb: refreshed facts/{} (path_hashes: {hash_count}, verified: {verified_commit} {verified_date})\n",
        args.id
    ))
}

#[derive(Debug, Clone)]
struct CheckReport {
    facts_count: usize,
    dead_ends_count: usize,
    baselines_count: usize,
    errors: Vec<String>,
    stale_warnings: Vec<String>,
}

impl CheckReport {
    fn passed(&self, fail_on_stale: bool) -> bool {
        self.errors.is_empty() && (!fail_on_stale || self.stale_warnings.is_empty())
    }

    fn render(&self) -> String {
        let mut out = String::new();
        push_fmt_line(
            &mut out,
            format_args!(
                "kb check: facts={}, dead_ends={}, baselines={}, errors={}, stale={}",
                self.facts_count,
                self.dead_ends_count,
                self.baselines_count,
                self.errors.len(),
                self.stale_warnings.len()
            ),
        );
        for warn in &self.stale_warnings {
            push_fmt_line(&mut out, format_args!("  stale: {warn}"));
        }
        for err in &self.errors {
            push_fmt_line(&mut out, format_args!("  error: {err}"));
        }
        out
    }
}

fn run_check(args: &CheckArgs) -> CheckReport {
    let root = Path::new(&args.root);
    let kb_dir = root.join(KB_DIR);
    if !kb_dir.is_dir() {
        return CheckReport {
            facts_count: 0,
            dead_ends_count: 0,
            baselines_count: 0,
            errors: vec![format!("KB directory does not exist: {}", kb_dir.display())],
            stale_warnings: Vec::new(),
        };
    }

    let facts = load_facts(root);
    let dead_ends = load_dead_ends(root);
    let baselines = load_baselines(root);

    let mut errors = Vec::new();
    errors.extend(facts.errors);
    errors.extend(dead_ends.errors);
    errors.extend(baselines.errors);

    let mut hash_cache = BTreeMap::new();
    let mut stale_warnings = Vec::new();
    for fact in &facts.entries {
        for tag in fact_staleness_tags(root, fact, &mut hash_cache) {
            stale_warnings.push(format!("{}: {tag}", fact.id));
        }
    }

    CheckReport {
        facts_count: facts.entries.len(),
        dead_ends_count: dead_ends.entries.len(),
        baselines_count: baselines.entries.len(),
        errors,
        stale_warnings,
    }
}

// ---------------------------------------------------------------------------
// Legacy agents/annoyances/*.json migration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct MigrateSummary {
    scanned: usize,
    migrated_facts: usize,
    migrated_dead_ends: usize,
    skipped_closed: usize,
    skipped_unanchored: usize,
    skipped_duplicate: usize,
    skipped_malformed: usize,
    total_facts: usize,
    total_dead_ends: usize,
}

impl MigrateSummary {
    fn render(&self) -> String {
        format!(
            "kb migrate_annoyances: scanned={}, migrated_facts={}, migrated_dead_ends={}, skipped_closed={}, skipped_unanchored={}, skipped_duplicate={}, skipped_malformed={} (total facts={}, dead_ends={})\n",
            self.scanned,
            self.migrated_facts,
            self.migrated_dead_ends,
            self.skipped_closed,
            self.skipped_unanchored,
            self.skipped_duplicate,
            self.skipped_malformed,
            self.total_facts,
            self.total_dead_ends
        )
    }
}

fn json_field_text(obj: &Map<String, Value>, key: &str) -> String {
    match obj.get(key) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(items)) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
                .collect();
            parts.join("; ")
        }
        _ => String::new(),
    }
}

fn is_trivial_text(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    lower.is_empty()
        || matches!(
            lower.as_str(),
            "none" | "none needed" | "none needed now" | "n/a" | "na" | "-" | "nothing"
        )
}

fn normalize_migrated_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '.' || lower == '_' || lower == '-' {
            if lower == '-' {
                if !prev_dash && !out.is_empty() {
                    out.push('-');
                    prev_dash = true;
                }
            } else {
                out.push(lower);
                prev_dash = false;
            }
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn first_sentence_summary(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let bytes = flat.as_bytes();
    let mut cut_byte = 0usize;
    for (count, (idx, ch)) in flat.char_indices().enumerate() {
        if count >= max_chars {
            break;
        }
        let end = idx + ch.len_utf8();
        if count >= 40 && (ch == '.' || ch == ';') {
            let prev_is_dot = idx > 0 && bytes[idx - 1] == b'.';
            let next_is_space = end < bytes.len() && bytes[end].is_ascii_whitespace();
            if !prev_is_dot && next_is_space {
                return flat[..end].to_owned();
            }
        }
        cut_byte = end;
    }
    let truncated = flat[..cut_byte].trim_end();
    format!("{truncated}...")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut_byte = 0usize;
    for (count, (idx, ch)) in text.char_indices().enumerate() {
        if count >= max_chars {
            break;
        }
        cut_byte = idx + ch.len_utf8();
    }
    format!("{}...", text[..cut_byte].trim_end())
}

fn strip_line_suffix(token: &str) -> &str {
    let without_anchor = token.split('#').next().unwrap_or(token);
    if let Some((base, tail)) = without_anchor.rsplit_once(':')
        && !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b':')
    {
        return base;
    }
    without_anchor
}

fn extract_crates_paths_symbols(
    root: &Path,
    texts: &[&str],
) -> (Vec<String>, Vec<String>, Vec<String>, BTreeMap<String, String>) {
    let mut crates = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let mut path_hashes = BTreeMap::new();

    for text in texts {
        // 1. Scan whitespace-delimited tokens for repo paths and crate names.
        for raw_tok in text.split_whitespace() {
            let trimmed = raw_tok.trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '<' | '>'
                )
            });
            let trimmed = trimmed.trim_end_matches(['.', ':']);
            let cand = strip_line_suffix(trimmed);

            if REPO_PATH_PREFIXES.iter().any(|p| cand.starts_with(p))
                && let Ok(norm) = normalize_rel_path(cand)
            {
                if let Some(after_crates) = norm.strip_prefix("crates/")
                    && let Some(crate_name) = after_crates.split('/').next()
                    && !crate_name.is_empty()
                {
                    crates.insert(crate_name.to_owned());
                }
                let full = root.join(&norm);
                if let Ok(meta) = std::fs::metadata(&full) {
                    if meta.is_file() {
                        if let Ok(bytes) = std::fs::read(&full) {
                            path_hashes.insert(norm.clone(), sha256_hex(&bytes));
                            paths.insert(norm);
                        }
                    } else if meta.is_dir() && norm.contains('/') {
                        paths.insert(norm);
                    }
                }
            }

            // Crate-name pattern detection (blaze-*, blaze_*, hir-*, rustc_*, bstd, scripts).
            for piece in trimmed.split(['/', ':', '(', ')', ',', ';']) {
                let p = piece.trim_matches(|c: char| {
                    !c.is_ascii_alphanumeric() && c != '-' && c != '_'
                });
                if p.len() < 4 || p.len() > 40 {
                    continue;
                }
                if p.starts_with("blaze-") || p.starts_with("hir-") {
                    crates.insert(p.to_ascii_lowercase());
                } else if let Some(rest) = p.strip_prefix("blaze_")
                    && !rest.is_empty()
                    && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    crates.insert(format!("blaze-{}", rest.replace('_', "-")));
                } else if p == "bstd" || p == "scripts" || p == "astlog" || p == "syntax-tree" {
                    crates.insert(p.to_owned());
                }
            }
        }

        // 2. Scan backticked spans for Rust symbol names.
        let mut rest = *text;
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else { break };
            let span = after[..close].trim();
            rest = &after[close + 1..];
            if symbols.len() >= 8 {
                continue;
            }
            if is_candidate_symbol(span) {
                symbols.insert(span.to_owned());
            }
        }
    }

    (
        crates.into_iter().collect(),
        paths.into_iter().collect(),
        symbols.into_iter().collect(),
        path_hashes,
    )
}

fn is_candidate_symbol(span: &str) -> bool {
    if span.len() < 3 || span.len() > 64 {
        return false;
    }
    if span.contains(' ') || span.contains('/') || span.contains('.') || span.starts_with('-') {
        return false;
    }
    let valid_chars = span
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':'));
    if !valid_chars {
        return false;
    }
    let has_path_sep = span.contains("::");
    let has_underscore = span.contains('_');
    let has_upper_and_lower =
        span.chars().any(|c| c.is_ascii_uppercase()) && span.chars().any(|c| c.is_ascii_lowercase());
    has_path_sep || has_underscore || has_upper_and_lower
}

fn looks_like_dead_end(obj: &Map<String, Value>, what: &str, cause: &str) -> bool {
    if !json_field_text(obj, "tried").is_empty() {
        return true;
    }
    let combined = format!("{what} {cause}").to_ascii_lowercase();
    combined.contains("dead end")
        || combined.contains("dead-end")
        || combined.contains("reverted")
        || combined.contains("abandoned")
}

fn run_migrate_annoyances(args: &MigrateArgs) -> Result<MigrateSummary, String> {
    let root = Path::new(&args.root);
    let legacy_dir = root.join(LEGACY_ANNOYANCES_DIR);
    if !legacy_dir.is_dir() {
        return Err(format!(
            "legacy annoyances directory does not exist: {}",
            legacy_dir.display()
        ));
    }

    let existing_facts = load_facts(root);
    if !existing_facts.errors.is_empty() {
        return Err(format!(
            "cannot migrate into corrupt facts.jsonl:\n  {}",
            existing_facts.errors.join("\n  ")
        ));
    }
    let existing_dead_ends = load_dead_ends(root);
    if !existing_dead_ends.errors.is_empty() {
        return Err(format!(
            "cannot migrate into corrupt dead_ends.jsonl:\n  {}",
            existing_dead_ends.errors.join("\n  ")
        ));
    }

    let mut facts_by_id: BTreeMap<String, FactEntry> = existing_facts
        .entries
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
    let mut dead_ends_by_id: BTreeMap<String, DeadEndEntry> = existing_dead_ends
        .entries
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();

    let mut json_files = Vec::new();
    let dir_iter = std::fs::read_dir(&legacy_dir)
        .map_err(|e| format!("failed to read {}: {e}", legacy_dir.display()))?;
    for entry_res in dir_iter {
        let entry = entry_res
            .map_err(|e| format!("failed to read entry in {}: {e}", legacy_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") && path.is_file() {
            json_files.push(path);
        }
    }
    json_files.sort();

    let mut summary = MigrateSummary::default();

    for path in json_files {
        summary.scanned += 1;
        let raw_text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                summary.skipped_malformed += 1;
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&raw_text) {
            Ok(v) => v,
            Err(_) => {
                summary.skipped_malformed += 1;
                continue;
            }
        };
        let Some(obj) = value.as_object() else {
            summary.skipped_malformed += 1;
            continue;
        };

        let state = {
            let s = json_field_text(obj, "state");
            if s.is_empty() { json_field_text(obj, "status") } else { s }
        };
        let state_lower = state.to_ascii_lowercase();
        let is_open = state_lower.is_empty()
            || state_lower == "open"
            || state_lower.starts_with("open:")
            || state_lower.starts_with("open ")
            || state_lower.contains("partly")
            || state_lower.contains("not compiled")
            || state_lower.contains("not built")
            || state_lower.contains("pending");

        let prevention = json_field_text(obj, "prevention");
        let workaround = json_field_text(obj, "workaround");
        let class = json_field_text(obj, "class");

        if !is_open {
            summary.skipped_closed += 1;
            continue;
        }

        let what = {
            let w = json_field_text(obj, "what");
            if w.is_empty() { json_field_text(obj, "annoyance") } else { w }
        };
        if what.is_empty() {
            summary.skipped_malformed += 1;
            continue;
        }

        let raw_id = {
            let id_field = json_field_text(obj, "id");
            if !id_field.is_empty() {
                id_field
            } else {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("migrated")
                    .to_owned()
            }
        };
        let norm_id = normalize_migrated_id(&raw_id);
        if norm_id.is_empty() {
            summary.skipped_malformed += 1;
            continue;
        }

        if facts_by_id.contains_key(&norm_id) || dead_ends_by_id.contains_key(&norm_id) {
            summary.skipped_duplicate += 1;
            continue;
        }

        let date_raw = json_field_text(obj, "date");
        let date = if is_iso_date(&date_raw) { date_raw } else { "1970-01-01".to_owned() };
        let where_field = json_field_text(obj, "where");
        let area_field = json_field_text(obj, "area");
        let cause = json_field_text(obj, "cause");
        let fix = json_field_text(obj, "fix");

        let (crates, paths, symbols, path_hashes) = extract_crates_paths_symbols(
            root,
            &[&where_field, &area_field, &what, &cause, &workaround, &prevention, &fix],
        );

        if looks_like_dead_end(obj, &what, &cause) {
            let tried_field = json_field_text(obj, "tried");
            let tried = if tried_field.is_empty() {
                first_sentence_summary(&what, 240)
            } else {
                truncate_chars(&tried_field, 400)
            };
            let measured_effect = {
                let cost = json_field_text(obj, "cost");
                if !cost.is_empty() {
                    truncate_chars(&cost, 300)
                } else {
                    truncate_chars(&what, 300)
                }
            };
            let root_cause = if !cause.is_empty() {
                truncate_chars(&cause, 400)
            } else {
                truncate_chars(&what, 400)
            };
            let dead_end = DeadEndEntry {
                id: norm_id.clone(),
                crates,
                paths,
                symbols,
                tried,
                measured_effect,
                root_cause,
                class: truncate_chars(&class, 160),
                commit: "migrated-annoyance".to_owned(),
                date,
            };
            dead_ends_by_id.insert(norm_id, dead_end);
            summary.migrated_dead_ends += 1;
        } else {
            if paths.is_empty() {
                summary.skipped_unanchored += 1;
                continue;
            }
            let mut detail_parts = Vec::new();
            detail_parts.push(truncate_chars(&what, 600));
            if !where_field.is_empty() {
                detail_parts.push(format!("where: {}", truncate_chars(&where_field, 200)));
            } else if !area_field.is_empty() {
                detail_parts.push(format!("area: {}", truncate_chars(&area_field, 200)));
            }
            if !cause.is_empty() {
                detail_parts.push(format!("cause: {}", truncate_chars(&cause, 300)));
            }
            if !is_trivial_text(&workaround) {
                detail_parts.push(format!("workaround: {}", truncate_chars(&workaround, 300)));
            }
            if !is_trivial_text(&prevention) {
                detail_parts.push(format!("prevention: {}", truncate_chars(&prevention, 300)));
            }
            if !is_trivial_text(&class) {
                detail_parts.push(format!("class: {}", truncate_chars(&class, 160)));
            }
            if !is_trivial_text(&fix) {
                detail_parts.push(format!("fix: {}", truncate_chars(&fix, 240)));
            }

            let kind = if norm_id.starts_with("h-") || norm_id.starts_with("howto-") {
                "howto".to_owned()
            } else {
                "gotcha".to_owned()
            };

            let fact = FactEntry {
                id: norm_id.clone(),
                kind,
                crates,
                paths,
                symbols,
                summary: first_sentence_summary(&what, 180),
                detail: detail_parts.join(" | "),
                verified_commit: "migrated-annoyance".to_owned(),
                verified_date: date,
                path_hashes,
            };
            facts_by_id.insert(norm_id, fact);
            summary.migrated_facts += 1;
        }
    }

    let mut facts_vec: Vec<FactEntry> = facts_by_id.into_values().collect();
    let mut dead_ends_vec: Vec<DeadEndEntry> = dead_ends_by_id.into_values().collect();
    save_facts(root, &mut facts_vec)?;
    save_dead_ends(root, &mut dead_ends_vec)?;

    let baselines_path = TableKind::Baselines.path_under(root);
    if !baselines_path.exists() {
        write_atomic(&baselines_path, "")?;
    }

    summary.total_facts = facts_vec.len();
    summary.total_dead_ends = dead_ends_vec.len();
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn write_stdout(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(err) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        fail(SCRIPT, &format!("stdout write failed: {err}"));
    }
}

fn write_stderr(text: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    if err.write_all(text.as_bytes()).and_then(|()| err.flush()).is_err() {}
}

// ---------------------------------------------------------------------------
// Selftest
// ---------------------------------------------------------------------------

fn selftest_write(path: &Path, content: &str) {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        fail(SCRIPT, &format!("selftest create_dir_all {}: {err}", parent.display()));
    }
    if let Err(err) = std::fs::write(path, content.as_bytes()) {
        fail(SCRIPT, &format!("selftest write {}: {err}", path.display()));
    }
}

fn parse_cmd_ok(val: &Value) -> Command {
    match parse_command_value(val) {
        Ok(cmd) => cmd,
        Err(InputError(msg)) => fail(SCRIPT, &format!("selftest parse failed unexpectedly: {msg}")),
    }
}

fn parse_add_ok(val: &Value) -> AddArgs {
    match parse_cmd_ok(val) {
        Command::Add(a) => *a,
        Command::Query(_)
        | Command::Refresh(_)
        | Command::Check(_)
        | Command::MigrateAnnoyances(_) => fail(SCRIPT, "selftest expected Add command"),
    }
}

fn parse_query_ok(val: &Value) -> QueryArgs {
    match parse_cmd_ok(val) {
        Command::Query(q) => *q,
        Command::Add(_)
        | Command::Refresh(_)
        | Command::Check(_)
        | Command::MigrateAnnoyances(_) => fail(SCRIPT, "selftest expected Query command"),
    }
}

fn parse_refresh_ok(val: &Value) -> RefreshArgs {
    match parse_cmd_ok(val) {
        Command::Refresh(r) => r,
        Command::Query(_)
        | Command::Add(_)
        | Command::Check(_)
        | Command::MigrateAnnoyances(_) => fail(SCRIPT, "selftest expected Refresh command"),
    }
}

fn selftest() {
    let tmp = TempDir::new("kb-selftest-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let root = tmp.path();
    let root_str = root.to_string_lossy().into_owned();

    // Seed workspace files for SHA-256 anchoring.
    let cli_main = root.join("crates/blaze-cli/src/main.rs");
    let cas_lib = root.join("crates/blaze-cas/src/lib.rs");
    selftest_write(&cli_main, "fn main() { /* v1 */ }\n");
    selftest_write(&cas_lib, "pub fn open_store() {}\n");

    // 1. add + canonical sort + duplicate-ID refusal + overwrite: true + unknown field refusal.
    expect(
        SCRIPT,
        parse_command_value(&serde_json::json!({
            "command": "add",
            "root": root_str,
            "table": "facts",
            "entry": {
                "id": "bad-unknown",
                "kind": "fact",
                "summary": "x",
                "detail": "y",
                "verified_commit": "abc1234",
                "verified_date": "2026-11-26",
                "unexpected_key": 1
            }
        }))
        .is_err(),
        "unknown field inside entry must be refused",
    );

    let add_z = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "facts",
        "entry": {
            "id": "z-cas-open",
            "kind": "fact",
            "crates": ["blaze-cas"],
            "paths": ["crates/blaze-cas/src/lib.rs"],
            "symbols": ["open_store"],
            "summary": "Lazy store open avoids mapping bloom eagerly",
            "detail": "Bloom mapping is deferred until unmapped probes run.",
            "verified_commit": "aaa1111",
            "verified_date": "2026-11-26"
        }
    }));
    let res = run_add(&add_z).unwrap_or_else(|e| fail(SCRIPT, &format!("add z failed: {e}")));
    expect(SCRIPT, res.contains("added facts/z-cas-open"), "add z reports added");

    let add_a = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "facts",
        "entry": {
            "id": "a-cli-entry",
            "kind": "gotcha",
            "crates": ["blaze-cli"],
            "paths": ["crates/blaze-cli/src/main.rs"],
            "symbols": ["Engine::check"],
            "summary": "CLI entrypoint uses exact startup clock",
            "detail": "Avoid calling malloc_zone_statistics twice on no-op check.",
            "verified_commit": "bbb2222",
            "verified_date": "2026-11-26"
        }
    }));
    let _ = run_add(&add_a).unwrap_or_else(|e| fail(SCRIPT, &format!("add a failed: {e}")));

    // Verify canonical sort by id and sorted keys in JSONL.
    let facts_raw = std::fs::read_to_string(TableKind::Facts.path_under(root))
        .unwrap_or_else(|e| fail(SCRIPT, &format!("read facts.jsonl: {e}")));
    let fact_lines: Vec<&str> = facts_raw.lines().collect();
    expect(SCRIPT, fact_lines.len() == 2, "facts.jsonl has 2 lines");
    expect(
        SCRIPT,
        fact_lines[0].contains("\"id\":\"a-cli-entry\"")
            && fact_lines[1].contains("\"id\":\"z-cas-open\""),
        "facts.jsonl is canonically sorted by id",
    );
    expect(
        SCRIPT,
        fact_lines[0].starts_with("{\"crates\":"),
        "facts.jsonl writes sorted keys",
    );

    // Duplicate ID refused by default.
    expect(
        SCRIPT,
        run_add(&add_a).is_err(),
        "duplicate id must be refused when overwrite is false",
    );

    // Overwrite succeeds and preserves line count.
    let mut add_a_overwrite = add_a.clone();
    add_a_overwrite.overwrite = true;
    let upd = run_add(&add_a_overwrite)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("overwrite failed: {e}")));
    expect(SCRIPT, upd.contains("updated facts/a-cli-entry"), "overwrite reports updated");

    // Missing anchored path fails loud.
    let add_missing = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "facts",
        "entry": {
            "id": "missing-path-fact",
            "kind": "fact",
            "paths": ["crates/does-not-exist/src/lib.rs"],
            "summary": "Missing file should fail",
            "detail": "Rule 3",
            "verified_commit": "ccc3333",
            "verified_date": "2026-11-26"
        }
    }));
    let missing_err = run_add(&add_missing);
    expect(
        SCRIPT,
        missing_err.as_ref().is_err_and(|e| e.contains("crates/does-not-exist/src/lib.rs")),
        "missing anchored path fails loud naming the path",
    );

    // Add a dead_end and a baseline for cross-table ranking tests.
    let add_de = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "dead_ends",
        "entry": {
            "id": "de-prefix-match",
            "crates": ["blaze-cli"],
            "paths": ["crates/blaze-cli"],
            "symbols": ["run_noop"],
            "tried": "Tried caching mmap handles across runs",
            "measured_effect": "+0.4 ms slower on warm check",
            "root_cause": "Extra lock contention on startup",
            "class": "premature-cache",
            "commit": "ddd4444",
            "date": "2026-11-26"
        }
    }));
    let _ = run_add(&add_de).unwrap_or_else(|e| fail(SCRIPT, &format!("add dead_end failed: {e}")));

    let add_crate_only = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "dead_ends",
        "entry": {
            "id": "de-crate-only",
            "crates": ["blaze-cli"],
            "paths": [],
            "symbols": [],
            "tried": "Tried eagerly spawning worker pool on noop",
            "measured_effect": "+1.2 ms startup",
            "root_cause": "Thread creation dominates 4ms budget",
            "class": "startup-overhead",
            "commit": "eee5555",
            "date": "2026-11-26"
        }
    }));
    let _ = run_add(&add_crate_only)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("add crate_only dead_end failed: {e}")));

    let add_sym_only = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "dead_ends",
        "entry": {
            "id": "de-symbol-only",
            "crates": ["other-crate"],
            "paths": [],
            "symbols": ["special_probe_sym"],
            "tried": "Tried binary search in tiny table",
            "measured_effect": "No speedup",
            "root_cause": "Branch misprediction",
            "class": "micro-opt",
            "commit": "fff6666",
            "date": "2026-11-26"
        }
    }));
    let _ = run_add(&add_sym_only)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("add sym_only dead_end failed: {e}")));

    let add_bl = parse_add_ok(&serde_json::json!({
        "command": "add",
        "root": root_str,
        "table": "baselines",
        "entry": {
            "id": "macos-arm64-noop-aaa1111",
            "machine": "macos-aarch64",
            "cores": 18,
            "commit": "aaa1111",
            "date": "2026-11-26",
            "workload": "noop_startup",
            "wall_ms": 4.69,
            "rss_mb": 7.9,
            "breakdown": [
                {"component": "dyld", "value": 1.81, "unit": "ms"},
                {"component": "store_open", "value": 0.62, "unit": "ms"}
            ],
            "floor_note": "startup floor keyword marker"
        }
    }));
    let _ = run_add(&add_bl).unwrap_or_else(|e| fail(SCRIPT, &format!("add baseline failed: {e}")));

    // 2. query ranking: exact path (+100) > prefix path (+60) > crate (+50) > symbol (+40) > text (+20).
    let q_rank = parse_query_ok(&serde_json::json!({
        "command": "query",
        "root": root_str,
        "paths": ["crates/blaze-cli/src/main.rs"],
        "crates": ["blaze-cli"],
        "symbols": ["special_probe_sym"],
        "text": "marker",
        "format": "json"
    }));
    let q_res = run_query(&q_rank).unwrap_or_else(|e| fail(SCRIPT, &format!("query failed: {e}")));
    let ids: Vec<&str> = q_res.hits.iter().map(|h| h.entry.id()).collect();
    expect(
        SCRIPT,
        ids == [
            "a-cli-entry",              // exact path (100) + crate (50) = 150
            "de-prefix-match",          // prefix path (60) + crate (50) = 110
            "de-crate-only",            // crate (50)
            "de-symbol-only",           // symbol (40)
            "macos-arm64-noop-aaa1111", // text (20)
        ],
        &format!("unexpected ranking order: {ids:?}"),
    );
    let q_json = render_query_json(&q_res)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("render_query_json failed: {e}")));
    expect(
        SCRIPT,
        serde_json::from_str::<Value>(&q_json).is_ok(),
        "query json output parses cleanly",
    );

    // 3. SHA-256 staleness lifecycle: fresh -> edit -> stale -> refresh -> fresh -> delete -> missing.
    let check_strict = CheckArgs { root: root_str.clone(), fail_on_stale: true };
    let chk_fresh = run_check(&check_strict);
    expect(
        SCRIPT,
        chk_fresh.passed(true) && chk_fresh.stale_warnings.is_empty(),
        &format!("fresh KB should pass strict check: {}", chk_fresh.render()),
    );

    // Edit anchored file -> [STALE: changed ... since bbb2222].
    selftest_write(&cli_main, "fn main() { /* v2 modified */ }\n");
    let q_stale_on = parse_query_ok(&serde_json::json!({
        "command": "query",
        "root": root_str,
        "paths": ["crates/blaze-cli/src/main.rs"],
        "tables": ["facts"],
        "include_stale": true
    }));
    let res_stale_on = run_query(&q_stale_on)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("query stale on failed: {e}")));
    expect(SCRIPT, res_stale_on.hits.len() == 1, "stale fact included when include_stale=true");
    expect(
        SCRIPT,
        res_stale_on.hits[0]
            .stale
            .iter()
            .any(|t| t == "[STALE: changed crates/blaze-cli/src/main.rs since bbb2222]"),
        &format!("expected changed stale tag, got {:?}", res_stale_on.hits[0].stale),
    );
    let text_out = render_query_text(&res_stale_on);
    expect(
        SCRIPT,
        text_out.contains("[STALE: changed crates/blaze-cli/src/main.rs since bbb2222]"),
        "text output surfaces stale tag",
    );

    let mut q_stale_off = q_stale_on.clone();
    q_stale_off.include_stale = false;
    let res_stale_off = run_query(&q_stale_off)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("query stale off failed: {e}")));
    expect(SCRIPT, res_stale_off.hits.is_empty(), "stale fact omitted when include_stale=false");

    let chk_stale = run_check(&check_strict);
    expect(
        SCRIPT,
        !chk_stale.passed(true) && chk_stale.passed(false) && chk_stale.stale_warnings.len() == 1,
        "check reports stale warning and respects fail_on_stale",
    );

    // Refresh updates path_hashes, commit, and date -> fresh again.
    let refresh_cmd = parse_refresh_ok(&serde_json::json!({
        "command": "refresh",
        "root": root_str,
        "id": "a-cli-entry",
        "commit": "999ffff",
        "date": "2026-11-27"
    }));
    let ref_out = run_refresh(&refresh_cmd)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("refresh failed: {e}")));
    expect(SCRIPT, ref_out.contains("999ffff"), "refresh reports updated commit");
    let chk_after_refresh = run_check(&check_strict);
    expect(
        SCRIPT,
        chk_after_refresh.passed(true) && chk_after_refresh.stale_warnings.is_empty(),
        "check is fresh after refresh",
    );

    // Delete anchored file -> [STALE: missing ...], refresh fails loud.
    if let Err(err) = std::fs::remove_file(&cli_main) {
        fail(SCRIPT, &format!("selftest remove_file failed: {err}"));
    }
    let res_missing = run_query(&q_stale_on)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("query missing failed: {e}")));
    expect(
        SCRIPT,
        res_missing.hits[0]
            .stale
            .iter()
            .any(|t| t == "[STALE: missing crates/blaze-cli/src/main.rs]"),
        &format!("expected missing stale tag, got {:?}", res_missing.hits[0].stale),
    );
    expect(
        SCRIPT,
        run_refresh(&refresh_cmd).is_err(),
        "refresh must fail loud when anchored file is deleted",
    );

    // 4. check catches duplicate IDs, unsorted lines, and bad schemas.
    let bad_tmp = TempDir::new("kb-selftest-bad-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let bad_root = bad_tmp.path();
    let bad_root_str = bad_root.to_string_lossy().into_owned();
    let line_b = r#"{"crates":[],"detail":"d","id":"b-id","kind":"fact","path_hashes":{},"paths":[],"summary":"s","symbols":[],"verified_commit":"abc","verified_date":"2026-11-26"}"#;
    let line_a = r#"{"crates":[],"detail":"d","id":"a-id","kind":"fact","path_hashes":{},"paths":[],"summary":"s","symbols":[],"verified_commit":"abc","verified_date":"2026-11-26"}"#;
    selftest_write(
        &TableKind::Facts.path_under(bad_root),
        &format!("{line_b}\n{line_a}\n{line_a}\n"),
    );
    selftest_write(
        &TableKind::Baselines.path_under(bad_root),
        "{\"id\":\"bad-bl\",\"machine\":\"m\",\"cores\":0,\"commit\":\"c\",\"date\":\"2026-99-99\",\"workload\":\"w\",\"wall_ms\":-1.0,\"rss_mb\":1.0,\"breakdown\":[],\"floor_note\":\"\",\"extra\":1}\n",
    );
    let bad_chk = run_check(&CheckArgs { root: bad_root_str, fail_on_stale: false });
    expect(
        SCRIPT,
        !bad_chk.passed(false),
        "check must fail on duplicate, unsorted, and bad schema lines",
    );
    let joined_errs = bad_chk.errors.join("\n");
    expect(SCRIPT, joined_errs.contains("unsorted id"), &format!("missing unsorted err: {joined_errs}"));
    expect(SCRIPT, joined_errs.contains("duplicate id"), &format!("missing duplicate err: {joined_errs}"));
    expect(SCRIPT, joined_errs.contains("baselines.jsonl:1"), &format!("missing schema err: {joined_errs}"));

    // 5. migrate_annoyances on a synthetic temp agents/annoyances/ fixture.
    let mig_tmp = TempDir::new("kb-selftest-mig-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let mig_root = mig_tmp.path();
    let mig_root_str = mig_root.to_string_lossy().into_owned();
    selftest_write(&mig_root.join("crates/blaze-cli/src/main.rs"), "fn main() {}\n");
    selftest_write(
        &mig_root.join("agents/annoyances/open-cli.json"),
        r#"{
  "id": "open-cli-gotcha",
  "date": "2026-11-20",
  "where": "crates/blaze-cli/src/main.rs (`Engine::check`)",
  "what": "Opening store twice allocates duplicate arenas.",
  "cause": "Two startup callers invoked open independently.",
  "workaround": "Pass the opened handle down.",
  "state": "open"
}"#,
    );
    selftest_write(
        &mig_root.join("agents/annoyances/fixed-with-prev.json"),
        r#"{
  "id": "fixed-with-prevention",
  "date": "2026-11-19",
  "where": "crates/blaze-cas",
  "what": "Corrupt bloom file was ignored silently.",
  "prevention": "Always run DST with planted CorruptBloom.",
  "state": "fixed"
}"#,
    );
    selftest_write(
        &mig_root.join("agents/annoyances/fixed-noise.json"),
        r#"{
  "id": "fixed-noise-row",
  "date": "2026-11-18",
  "where": "crates/blaze-cli",
  "what": "Typo in comment.",
  "workaround": "none",
  "state": "fixed"
}"#,
    );
    selftest_write(
        &mig_root.join("agents/annoyances/reverted-exp.json"),
        r#"{
  "id": "reverted-mmap-cache",
  "date": "2026-11-21",
  "where": "crates/blaze-cli/src/main.rs",
  "what": "Tried global mmap cache and reverted because warm check regressed by 1.1 ms.",
  "cause": "Extra lock on hot path.",
  "class": "dead-end-cache",
  "state": "open"
}"#,
    );
    selftest_write(
        &mig_root.join("agents/annoyances/open-unanchored.json"),
        r#"{
  "id": "open-unanchored-note",
  "date": "2026-11-18",
  "where": "deleted/legacy/harness.py",
  "what": "Ephemeral lane note referencing no live file in the repo.",
  "state": "open"
}"#,
    );
    selftest_write(
        &mig_root.join("agents/annoyances/broken.json"),
        "{ not valid json",
    );

    let mig_res = run_migrate_annoyances(&MigrateArgs { root: mig_root_str.clone() })
        .unwrap_or_else(|e| fail(SCRIPT, &format!("migrate_annoyances failed: {e}")));
    expect(
        SCRIPT,
        mig_res.scanned == 6
            && mig_res.migrated_facts == 1
            && mig_res.migrated_dead_ends == 1
            && mig_res.skipped_closed == 2
            && mig_res.skipped_unanchored == 1
            && mig_res.skipped_malformed == 1,
        &format!("unexpected migrate summary: {}", mig_res.render()),
    );

    // Verify migrated KB passes strict check (all anchored files hashed cleanly).
    let mig_chk = run_check(&CheckArgs { root: mig_root_str.clone(), fail_on_stale: true });
    expect(
        SCRIPT,
        mig_chk.passed(true) && mig_chk.facts_count == 1 && mig_chk.dead_ends_count == 1,
        &format!("migrated KB failed strict check: {}", mig_chk.render()),
    );

    // Verify migrated fact extracted crates, paths, symbols, and path_hashes.
    let mig_facts = load_facts(mig_root);
    let cli_fact = mig_facts
        .entries
        .iter()
        .find(|f| f.id == "open-cli-gotcha")
        .unwrap_or_else(|| fail(SCRIPT, "missing migrated open-cli-gotcha"));
    expect(
        SCRIPT,
        cli_fact.crates.iter().any(|c| c == "blaze-cli")
            && cli_fact.paths.iter().any(|p| p == "crates/blaze-cli/src/main.rs")
            && cli_fact.symbols.iter().any(|s| s == "Engine::check")
            && cli_fact.path_hashes.contains_key("crates/blaze-cli/src/main.rs"),
        &format!("migrated fact missing extracted anchors: {cli_fact:?}"),
    );

    // Verify idempotency on second migrate run.
    let mig_res_2 = run_migrate_annoyances(&MigrateArgs { root: mig_root_str })
        .unwrap_or_else(|e| fail(SCRIPT, &format!("second migrate_annoyances failed: {e}")));
    expect(
        SCRIPT,
        mig_res_2.migrated_facts == 0
            && mig_res_2.migrated_dead_ends == 0
            && mig_res_2.skipped_duplicate == 2
            && mig_res_2.total_facts == 1
            && mig_res_2.total_dead_ends == 1,
        &format!("second migrate run must be idempotent: {}", mig_res_2.render()),
    );

    drop(mig_tmp);
    drop(bad_tmp);
    drop(tmp);
    write_stdout(&format!("{SCRIPT} selftest: ok\n"));
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

fn main_with(cmd: &Command) {
    match cmd {
        Command::Query(args) => {
            let result = match run_query(args) {
                Ok(r) => r,
                Err(err) => fail(SCRIPT, &err),
            };
            let out = match args.format {
                QueryFormat::Text => render_query_text(&result),
                QueryFormat::Json => match render_query_json(&result) {
                    Ok(s) => s,
                    Err(err) => fail(SCRIPT, &err),
                },
            };
            write_stdout(&out);
        }
        Command::Add(args) => match run_add(args) {
            Ok(msg) => write_stdout(&msg),
            Err(err) => fail(SCRIPT, &err),
        },
        Command::Refresh(args) => match run_refresh(args) {
            Ok(msg) => write_stdout(&msg),
            Err(err) => fail(SCRIPT, &err),
        },
        Command::Check(args) => {
            let report = run_check(args);
            let text = report.render();
            if report.passed(args.fail_on_stale) {
                write_stdout(&text);
            } else {
                write_stderr(&text);
                exit(1);
            }
        }
        Command::MigrateAnnoyances(args) => match run_migrate_annoyances(args) {
            Ok(summary) => write_stdout(&summary.render()),
            Err(err) => fail(SCRIPT, &err),
        },
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => {
            let value = match input::read(one) {
                Ok(v) => v,
                Err(InputError(msg)) => fail(SCRIPT, &msg),
            };
            let cmd = match parse_command_value(&value) {
                Ok(c) => c,
                Err(InputError(msg)) => fail(SCRIPT, &msg),
            };
            main_with(&cmd);
        }
        _ => {
            write_stderr(&format!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}"));
            exit(1);
        }
    }
}
