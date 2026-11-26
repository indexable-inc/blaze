//! `blaze-data::Value` builders and compact `blaze-tab` / `blaze-json` renderers.
//!
//! Every command result converts into a structured `blaze_data::Value` shaped for automatic
//! sparse/homogeneous columnar table compression in `blaze-tab` (`[[col1 col2 ...]; [row1] ...]`).

use core::fmt;

use blaze_data::{Int, Map, Value};
use blaze_json::{BytesAs, KeyOrder as JsonKeyOrder, Style as JsonStyle, WriteOptions as JsonWriteOptions};
use blaze_own::{AString, AVec, Alloc};
use blaze_tab::{KeyOrder as TabKeyOrder, Style as TabStyle, TableMode, WriteOptions as TabWriteOptions};

use crate::search::{AreaReport, OutlineReport, SearchResult};
use crate::tree::{DomainSummary, IndexStats};

/// Output encoding requested by the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    /// Compact LLM-first `blaze-tab` format (default).
    Tab,
    /// Standard RFC 8259 JSON via `blaze-json`.
    Json,
}

impl OutputFormat {
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "tab" | "blaze-tab" => Some(OutputFormat::Tab),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}

/// Errors formatting a [`Value`] as `blaze-tab` or JSON.
pub enum FormatError<'a> {
    Tab(blaze_tab::WriteError<Alloc<'a>>),
    Json(blaze_json::WriteError<Alloc<'a>>),
}

impl fmt::Display for FormatError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Tab(e) => write!(f, "blaze-tab format error: {e}"),
            FormatError::Json(e) => write!(f, "blaze-json format error: {e}"),
        }
    }
}

impl fmt::Debug for FormatError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FormatError({self})")
    }
}

impl core::error::Error for FormatError<'_> {}

/// Renders any [`blaze_data::Value`] as compact tabular `blaze-tab` or pretty `blaze-json`.
pub fn render_value<'a>(
    value: &Value<Alloc<'a>>,
    format: OutputFormat,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, FormatError<'a>> {
    match format {
        OutputFormat::Tab => {
            let opts = TabWriteOptions {
                style: TabStyle::Pretty { indent: 2 },
                keys: TabKeyOrder::Insertion,
                tables: TableMode::Auto { min_coverage_pct: 50 },
                most_depth: blaze_tab::Limits::MOST_DEPTH,
            };
            blaze_tab::write_with(value, opts, alloc).map_err(FormatError::Tab)
        }
        OutputFormat::Json => {
            let opts = JsonWriteOptions {
                style: JsonStyle::Pretty { indent: 2 },
                keys: JsonKeyOrder::Insertion,
                bytes: BytesAs::Refuse,
            };
            blaze_json::to_string_in(value, opts, alloc).map_err(FormatError::Json)
        }
    }
}

/// Converts a [`SearchResult`] into a tabular [`Value`] (`hits` + `sections` union-schema tables).
pub fn search_result_to_value<'a>(res: &SearchResult<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut root = Map::with_capacity_in(7, alloc);
    root.insert(AString::from_str_in("query", alloc), Value::Text(AString::from_str_in(res.query.as_str(), alloc)));
    root.insert(
        AString::from_str_in("budget_tokens", alloc),
        Value::Int(Int::from_u64(res.budget_tokens as u64)),
    );
    root.insert(
        AString::from_str_in("used_tokens", alloc),
        Value::Int(Int::from_u64(res.used_tokens as u64)),
    );
    root.insert(
        AString::from_str_in("total_candidates", alloc),
        Value::Int(Int::from_u64(res.total_candidates as u64)),
    );
    root.insert(
        AString::from_str_in("omitted", alloc),
        Value::Int(Int::from_u64(res.omitted as u64)),
    );

    let mut hit_rows = AVec::with_capacity_in(res.hits.len(), alloc);
    for h in res.hits.as_slice() {
        let mut row = Map::with_capacity_in(9, alloc);
        row.insert(AString::from_str_in("rank", alloc), Value::Int(Int::from_u64(u64::from(h.rank))));
        row.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(h.kind.as_str(), alloc)));
        row.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(h.status.as_str(), alloc)));
        row.insert(AString::from_str_in("score", alloc), Value::Float(h.score));
        row.insert(AString::from_str_in("match", alloc), Value::Text(AString::from_str_in(h.match_source.as_str(), alloc)));
        row.insert(AString::from_str_in("id", alloc), Value::Text(AString::from_str_in(h.id.as_str(), alloc)));
        row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(h.ptr.as_str(), alloc)));
        row.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(h.summary.as_str(), alloc)));
        if let Some(via) = &h.via {
            row.insert(AString::from_str_in("via", alloc), Value::Text(AString::from_str_in(via.as_str(), alloc)));
        }
        hit_rows.push(Value::Map(row));
    }
    root.insert(AString::from_str_in("hits", alloc), Value::List(hit_rows));

    if !res.sections.is_empty() {
        let mut sec_rows = AVec::with_capacity_in(res.sections.len(), alloc);
        for s in res.sections.as_slice() {
            let mut row = Map::with_capacity_in(4, alloc);
            row.insert(
                AString::from_str_in("status", alloc),
                Value::Text(AString::from_str_in(s.doc_status.as_str(), alloc)),
            );
            row.insert(AString::from_str_in("score", alloc), Value::Float(s.score));
            row.insert(
                AString::from_str_in("heading", alloc),
                Value::Text(AString::from_str_in(s.heading.as_str(), alloc)),
            );
            row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(s.ptr.as_str(), alloc)));
            sec_rows.push(Value::Map(row));
        }
        root.insert(AString::from_str_in("sections", alloc), Value::List(sec_rows));
    }

    Value::Map(root)
}

/// Converts the Level-0 Domain Map into a tabular [`Value`].
pub fn domain_map_to_value<'a>(
    domains: &[DomainSummary<'_>],
    total_crates: usize,
    total_docs: usize,
    total_notes: usize,
    total_edges: usize,
    alloc: Alloc<'a>,
) -> Value<Alloc<'a>> {
    let mut root = Map::with_capacity_in(5, alloc);
    root.insert(AString::from_str_in("crates", alloc), Value::Int(Int::from_u64(total_crates as u64)));
    root.insert(AString::from_str_in("docs", alloc), Value::Int(Int::from_u64(total_docs as u64)));
    root.insert(AString::from_str_in("notes", alloc), Value::Int(Int::from_u64(total_notes as u64)));
    root.insert(AString::from_str_in("edges", alloc), Value::Int(Int::from_u64(total_edges as u64)));

    let mut rows = AVec::with_capacity_in(domains.len(), alloc);
    for d in domains {
        let mut row = Map::with_capacity_in(9, alloc);
        row.insert(AString::from_str_in("domain", alloc), Value::Text(AString::from_str_in(d.domain.as_str(), alloc)));
        row.insert(AString::from_str_in("crates", alloc), Value::Int(Int::from_u64(u64::from(d.crate_count))));
        row.insert(AString::from_str_in("live", alloc), Value::Int(Int::from_u64(u64::from(d.live_crates))));
        row.insert(AString::from_str_in("parked", alloc), Value::Int(Int::from_u64(u64::from(d.parked_crates))));
        row.insert(AString::from_str_in("exp", alloc), Value::Int(Int::from_u64(u64::from(d.experimental_crates))));
        row.insert(AString::from_str_in("docs", alloc), Value::Int(Int::from_u64(u64::from(d.doc_count))));
        row.insert(AString::from_str_in("notes", alloc), Value::Int(Int::from_u64(u64::from(d.note_count))));
        row.insert(AString::from_str_in("key_crates", alloc), str_list_val(d.key_crates.as_slice(), alloc));
        row.insert(AString::from_str_in("key_docs", alloc), str_list_val(d.key_docs.as_slice(), alloc));
        rows.push(Value::Map(row));
    }
    root.insert(AString::from_str_in("domains", alloc), Value::List(rows));
    Value::Map(root)
}

/// Converts a document [`OutlineReport`] into a tabular [`Value`].
pub fn outline_to_value<'a>(report: &OutlineReport<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut root = Map::with_capacity_in(7, alloc);
    root.insert(AString::from_str_in("path", alloc), Value::Text(AString::from_str_in(report.path.as_str(), alloc)));
    root.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(report.status.as_str(), alloc)));
    if let Some(date) = &report.date {
        root.insert(AString::from_str_in("date", alloc), Value::Text(AString::from_str_in(date.as_str(), alloc)));
    }
    root.insert(AString::from_str_in("title", alloc), Value::Text(AString::from_str_in(report.title.as_str(), alloc)));
    root.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(report.summary.as_str(), alloc)));
    root.insert(AString::from_str_in("lines", alloc), Value::Int(Int::from_u64(u64::from(report.line_count))));

    let mut rows = AVec::with_capacity_in(report.headings.len(), alloc);
    for h in report.headings.as_slice() {
        let mut row = Map::with_capacity_in(4, alloc);
        row.insert(AString::from_str_in("level", alloc), Value::Int(Int::from_u64(u64::from(h.level))));
        row.insert(AString::from_str_in("heading", alloc), Value::Text(AString::from_str_in(h.heading.as_str(), alloc)));
        row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(h.ptr.as_str(), alloc)));
        row.insert(AString::from_str_in("lines", alloc), Value::Int(Int::from_u64(u64::from(h.lines))));
        rows.push(Value::Map(row));
    }
    root.insert(AString::from_str_in("headings", alloc), Value::List(rows));
    Value::Map(root)
}

/// Converts an [`AreaReport`] into a tabular [`Value`].
pub fn area_to_value<'a>(report: &AreaReport<'_>, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut root = Map::with_capacity_in(10, alloc);
    root.insert(AString::from_str_in("target", alloc), Value::Text(AString::from_str_in(report.target.as_str(), alloc)));
    root.insert(AString::from_str_in("domain", alloc), Value::Text(AString::from_str_in(report.domain.as_str(), alloc)));

    if report.found_crate {
        if let Some(name) = &report.crate_name {
            root.insert(AString::from_str_in("crate", alloc), Value::Text(AString::from_str_in(name.as_str(), alloc)));
        }
        if let Some(st) = &report.status {
            root.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(st.as_str(), alloc)));
        }
        if let Some(ptr) = &report.ptr {
            root.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(ptr.as_str(), alloc)));
        }
        if let Some(sum) = &report.summary {
            root.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(sum.as_str(), alloc)));
        }
        root.insert(AString::from_str_in("deps", alloc), str_list_val(report.deps.as_slice(), alloc));
        root.insert(AString::from_str_in("dependents", alloc), str_list_val(report.dependents.as_slice(), alloc));

        let mut exp_rows = AVec::with_capacity_in(report.exports.len(), alloc);
        for ex in report.exports.as_slice() {
            let mut row = Map::with_capacity_in(3, alloc);
            row.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(ex.kind.as_str(), alloc)));
            row.insert(AString::from_str_in("name", alloc), Value::Text(AString::from_str_in(ex.name.as_str(), alloc)));
            row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(ex.ptr.as_str(), alloc)));
            exp_rows.push(Value::Map(row));
        }
        root.insert(AString::from_str_in("exports", alloc), Value::List(exp_rows));
    }

    let mut doc_rows = AVec::with_capacity_in(report.design_docs.len(), alloc);
    for d in report.design_docs.as_slice() {
        let mut row = Map::with_capacity_in(5, alloc);
        row.insert(AString::from_str_in("status", alloc), Value::Text(AString::from_str_in(d.status.as_str(), alloc)));
        row.insert(AString::from_str_in("rel", alloc), Value::Text(AString::from_str_in(d.relation.as_str(), alloc)));
        row.insert(AString::from_str_in("title", alloc), Value::Text(AString::from_str_in(d.title.as_str(), alloc)));
        row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(d.ptr.as_str(), alloc)));
        doc_rows.push(Value::Map(row));
    }
    root.insert(AString::from_str_in("design_docs", alloc), Value::List(doc_rows));

    let mut note_rows = AVec::with_capacity_in(report.notes.len(), alloc);
    for n in report.notes.as_slice() {
        let mut row = Map::with_capacity_in(5, alloc);
        row.insert(AString::from_str_in("kind", alloc), Value::Text(AString::from_str_in(n.kind.as_str(), alloc)));
        row.insert(AString::from_str_in("id", alloc), Value::Text(AString::from_str_in(n.note_id.as_str(), alloc)));
        row.insert(AString::from_str_in("state", alloc), Value::Text(AString::from_str_in(n.state.as_str(), alloc)));
        row.insert(AString::from_str_in("ptr", alloc), Value::Text(AString::from_str_in(n.ptr.as_str(), alloc)));
        row.insert(AString::from_str_in("summary", alloc), Value::Text(AString::from_str_in(n.summary.as_str(), alloc)));
        note_rows.push(Value::Map(row));
    }
    root.insert(AString::from_str_in("notes", alloc), Value::List(note_rows));

    Value::Map(root)
}

/// Converts [`IndexStats`] plus cache path/size/elapsed timing into a [`Value`].
pub fn index_stats_to_value<'a>(
    stats: IndexStats,
    cache_path: &str,
    cache_bytes: usize,
    elapsed_ms: f64,
    alloc: Alloc<'a>,
) -> Value<Alloc<'a>> {
    let mut root = Map::with_capacity_in(11, alloc);
    root.insert(AString::from_str_in("cache", alloc), Value::Text(AString::from_str_in(cache_path, alloc)));
    root.insert(AString::from_str_in("cache_bytes", alloc), Value::Int(Int::from_u64(cache_bytes as u64)));
    root.insert(AString::from_str_in("elapsed_ms", alloc), Value::Float(crate::math::round3(elapsed_ms)));
    root.insert(AString::from_str_in("domains", alloc), Value::Int(Int::from_u64(u64::from(stats.total_domains))));
    root.insert(AString::from_str_in("crates", alloc), Value::Int(Int::from_u64(u64::from(stats.total_crates))));
    root.insert(AString::from_str_in("docs", alloc), Value::Int(Int::from_u64(u64::from(stats.total_docs))));
    root.insert(AString::from_str_in("notes", alloc), Value::Int(Int::from_u64(u64::from(stats.total_notes))));
    root.insert(AString::from_str_in("edges", alloc), Value::Int(Int::from_u64(u64::from(stats.total_edges))));
    root.insert(AString::from_str_in("reused", alloc), Value::Int(Int::from_u64(u64::from(stats.reused))));
    root.insert(AString::from_str_in("reindexed", alloc), Value::Int(Int::from_u64(u64::from(stats.reindexed))));
    root.insert(AString::from_str_in("removed", alloc), Value::Int(Int::from_u64(u64::from(stats.removed))));
    Value::Map(root)
}

fn str_list_val<'a>(list: &[AString<'_>], alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let mut out = AVec::with_capacity_in(list.len(), alloc);
    for s in list {
        out.push(Value::Text(AString::from_str_in(s.as_str(), alloc)));
    }
    Value::List(out)
}
