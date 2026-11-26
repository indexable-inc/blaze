//! `blaze-preview`: `#![no_std]` `#![blaze::sans_io]` `#![forbid(unsafe_code)]` rich file
//! preview engine powering `blaze-shell`'s `y` (Yazi) file manager and hover popovers.
//!
//! Turns raw file bytes + filename into the best structured visual representation:
//! - **Tabular previews** for lockfiles (`Cargo.lock`, `uv.lock`, `blaze.lock`), CSV/TSV,
//!   JSON arrays of objects, and TOML/JSON manifests (`Cargo.toml`, `package.json`, `flake.lock`),
//! - **Rendered Markdown** (`README.md`, `AGENTS.md`, `*.md`) with headings, bullets, inline
//!   code chips, bold/italic, and syntax-highlighted code fences,
//! - **Syntax-highlighted source code** for Rust, TS/JS, Python, Go, C/C++, Nix, Shell, SQL, etc.,
//! - **Structured binary metadata** (Mach-O/ELF arch, PNG/GIF dimensions, SQLite, Parquet, Zstd).

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

pub mod highlight;
pub mod markdown;

use core::fmt::Write;

use blaze_data::Value as DataValue;
use blaze_own::{AString, AVec, Alloc};
use blaze_sniff::{FileCategory, SniffResult, sniff_file};

pub use blaze_html::{ColorScheme, TextAlign};
pub use crate::highlight::{CodeLine, CodeSpan, TokenKind, highlight_code};
pub use crate::markdown::{
    MdBlock, MdSpan, MdStyle, MdTable, parse_html_document, parse_markdown,
    parse_markdown_with_scheme,
};

/// Limits guarding preview parsing & rendering against huge files (rule 18: bounded progress).
#[derive(Clone, Copy, Debug)]
pub struct PreviewLimits {
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_lines: usize,
    pub max_cell_chars: usize,
}

impl PreviewLimits {
    pub const SHIPPED: Self = Self {
        max_rows: 48,
        max_cols: 6,
        max_lines: 64,
        max_cell_chars: 64,
    };
}

/// Structured tabular preview (used for `uv.lock`, `Cargo.lock`, `blaze.lock`, CSV, JSON arrays).
#[derive(Clone, Debug)]
pub struct PreviewTable<'a> {
    pub title: AString<'a>,
    pub subtitle: AString<'a>,
    pub headers: AVec<'a, AString<'a>>,
    pub rows: AVec<'a, AVec<'a, AString<'a>>>,
    pub total_rows: usize,
}

/// Key-value section summary (used for `Cargo.toml`, `package.json`, `flake.lock`, binaries).
#[derive(Clone, Debug)]
pub struct PreviewKv<'a> {
    pub title: AString<'a>,
    pub subtitle: AString<'a>,
    pub pairs: AVec<'a, (AString<'a>, AString<'a>)>,
    /// Optional subordinate table (e.g. `[dependencies]` in `Cargo.toml` or inputs in `flake.lock`).
    pub table: Option<PreviewTable<'a>>,
}

/// Rich structured preview body returned by [`build_preview`].
#[derive(Clone, Debug)]
pub enum PreviewBody<'a> {
    Table(PreviewTable<'a>),
    KeyValues(PreviewKv<'a>),
    Markdown {
        blocks: AVec<'a, MdBlock<'a>>,
        truncated: bool,
    },
    Code {
        lang: &'static str,
        lines: AVec<'a, CodeLine<'a>>,
        truncated: bool,
    },
    Summary {
        pairs: AVec<'a, (AString<'a>, AString<'a>)>,
    },
}

/// Complete preview document combining `blaze-sniff` analysis + best display representation.
#[derive(Clone, Debug)]
pub struct PreviewDoc<'a> {
    pub sniff: SniffResult,
    pub file_size: u64,
    pub body: PreviewBody<'a>,
}

/// Build the richest structured preview for `(name, bytes, total_file_size)` in `alloc` (zero IO!).
pub fn build_preview<'a>(
    name: &str,
    bytes: &[u8],
    total_file_size: u64,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> PreviewDoc<'a> {
    let sniff = sniff_file(name, bytes);

    // 1. Binary formats: crisp structured summary card from `blaze-sniff`.
    if sniff.is_binary {
        let mut pairs = AVec::new_in(alloc);
        let mut kind_s = AString::new_in(alloc);
        let _ = sniff.format_label(&mut kind_s);
        pairs.push((AString::from_str_in("format", alloc), kind_s));
        if let Some((w, h)) = sniff.dims {
            let mut ds = AString::new_in(alloc);
            let _ = write!(ds, "{w} × {h} px");
            pairs.push((AString::from_str_in("dimensions", alloc), ds));
        }
        let mut sz = AString::new_in(alloc);
        let _ = write!(sz, "{total_file_size} bytes");
        pairs.push((AString::from_str_in("size", alloc), sz));
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Summary { pairs },
        };
    }

    let Ok(text) = core::str::from_utf8(bytes) else {
        let mut pairs = AVec::new_in(alloc);
        pairs.push((
            AString::from_str_in("format", alloc),
            AString::from_str_in("Non-UTF-8 binary", alloc),
        ));
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Summary { pairs },
        };
    };

    let truncated = (bytes.len() as u64) < total_file_size;

    // 2. Rendered Markdown (`README.md`, `AGENTS.md`, `*.md`) & standalone HTML (`*.html`, `*.htm`).
    if sniff.label == "Markdown" {
        let blocks = parse_markdown(text, limits.max_lines, alloc);
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Markdown { blocks, truncated },
        };
    }
    if sniff.label == "HTML" {
        let blocks = parse_html_document(text, ColorScheme::Dark, limits.max_lines, alloc);
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Markdown { blocks, truncated },
        };
    }

    // 3. Lockfiles (`uv.lock`, `Cargo.lock`, `blaze.lock`) -> crisp package table!
    if matches!(sniff.label, "Cargo lock" | "uv lock" | "Blaze lock")
        && let Some(tbl) = preview_lockfile_packages(sniff.label, text, limits, alloc)
    {
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Table(tbl),
        };
    }

    // 4. CSV / TSV -> crisp tabular preview!
    if matches!(sniff.label, "CSV" | "TSV") {
        let sep = if sniff.label == "TSV" { b'\t' } else { b',' };
        if let Some(tbl) = preview_delimited(sniff.label, text, sep, limits, alloc) {
            return PreviewDoc {
                sniff,
                file_size: total_file_size,
                body: PreviewBody::Table(tbl),
            };
        }
    }

    // 5. TOML manifests (`Cargo.toml`, `pyproject.toml`, `.clippy.toml`, `*.toml`) -> structured view!
    if matches!(
        sniff.label,
        "Cargo manifest" | "Python proj" | "TOML" | "Clippy config" | "Rustfmt config"
    ) && !truncated
        && let Some(body) = preview_toml(sniff.label, text, limits, alloc)
    {
        return PreviewDoc { sniff, file_size: total_file_size, body };
    }

    // 6. JSON / `package.json` / `flake.lock` -> structured table or key-value view!
    if matches!(sniff.label, "JSON" | "Node manifest" | "TS config" | "Nix lock")
        && !truncated
        && let Some(body) = preview_json(sniff.label, text, limits, alloc)
    {
        return PreviewDoc { sniff, file_size: total_file_size, body };
    }

    // 7. JSONL -> tabular rows if objects share keys!
    if sniff.label == "JSONL"
        && let Some(tbl) = preview_jsonl(text, limits, alloc)
    {
        return PreviewDoc {
            sniff,
            file_size: total_file_size,
            body: PreviewBody::Table(tbl),
        };
    }

    // 8. Syntax-highlighted code / config / text fallback!
    let lines = highlight_code(text, sniff.label, limits.max_lines, alloc);
    let more_lines = text.lines().nth(limits.max_lines).is_some();
    PreviewDoc {
        sniff,
        file_size: total_file_size,
        body: PreviewBody::Code {
            lang: if sniff.category == FileCategory::Text { "Plain text" } else { sniff.label },
            lines,
            truncated: truncated || more_lines,
        },
    }
}

/// Stream-scan `[[package]]` entries from `Cargo.lock` / `uv.lock` / `blaze.lock` (works even when
/// only a prefix of a huge lockfile was read!).
fn preview_lockfile_packages<'a>(
    label: &str,
    text: &str,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewTable<'a>> {
    let mut rows: AVec<'a, AVec<'a, AString<'a>>> = AVec::new_in(alloc);
    let mut in_pkg = false;
    let mut cur_name: Option<&str> = None;
    let mut cur_ver: Option<&str> = None;
    let mut cur_src: Option<&str> = None;
    let mut total = 0usize;

    let mut flush = |name: &mut Option<&str>,
                     ver: &mut Option<&str>,
                     src: &mut Option<&str>,
                     rows: &mut AVec<'a, AVec<'a, AString<'a>>>| {
        if let Some(n) = name.take() {
            total += 1;
            if rows.len() < limits.max_rows {
                let mut row = AVec::with_capacity_in(3, alloc);
                row.push(AString::from_str_in(n, alloc));
                row.push(AString::from_str_in(ver.unwrap_or("—"), alloc));
                let src_short = short_source(src.unwrap_or("workspace"));
                row.push(AString::from_str_in(src_short, alloc));
                rows.push(row);
            }
        }
        *ver = None;
        *src = None;
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line == "[[package]]" || line == "[[crate]]" {
            if in_pkg {
                flush(&mut cur_name, &mut cur_ver, &mut cur_src, &mut rows);
            }
            in_pkg = true;
            continue;
        }
        if !in_pkg {
            continue;
        }
        if line.starts_with('[') && !line.starts_with("[[package]]") {
            flush(&mut cur_name, &mut cur_ver, &mut cur_src, &mut rows);
            in_pkg = false;
            continue;
        }
        if let Some(v) = extract_toml_str_field(line, "name") {
            cur_name = Some(v);
        } else if let Some(v) = extract_toml_str_field(line, "version") {
            cur_ver = Some(v);
        } else if let Some(v) = extract_toml_str_field(line, "source") {
            cur_src = Some(v);
        } else if line.starts_with("source = {") {
            if line.contains("pypi.org") {
                cur_src = Some("pypi");
            } else if line.contains("editable") {
                cur_src = Some("editable");
            } else if line.contains("virtual") {
                cur_src = Some("workspace");
            } else if line.contains("git") {
                cur_src = Some("git");
            }
        }
    }
    if in_pkg {
        flush(&mut cur_name, &mut cur_ver, &mut cur_src, &mut rows);
    }

    if rows.is_empty() {
        return None;
    }

    let mut headers = AVec::with_capacity_in(3, alloc);
    headers.push(AString::from_str_in("package", alloc));
    headers.push(AString::from_str_in("version", alloc));
    headers.push(AString::from_str_in("source", alloc));
    let mut sub = AString::new_in(alloc);
    let _ = write!(sub, "{total} packages");
    Some(PreviewTable {
        title: AString::from_str_in(label, alloc),
        subtitle: sub,
        headers,
        rows,
        total_rows: total,
    })
}

fn short_source(s: &str) -> &str {
    if s.contains("crates.io") {
        "crates.io"
    } else if s.contains("pypi.org") {
        "pypi"
    } else if s.starts_with("git+") {
        "git"
    } else {
        s
    }
}

fn extract_toml_str_field<'b>(line: &'b str, key: &str) -> Option<&'b str> {
    let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn preview_delimited<'a>(
    label: &str,
    text: &str,
    sep: u8,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewTable<'a>> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?;
    let mut headers = AVec::new_in(alloc);
    for col in first.split(sep as char).take(limits.max_cols) {
        headers.push(AString::from_str_in(col.trim().trim_matches('"'), alloc));
    }
    if headers.len() < 2 {
        return None;
    }
    let ncols = headers.len();
    let mut rows = AVec::new_in(alloc);
    let mut total = 0usize;
    for line in lines {
        total += 1;
        if rows.len() < limits.max_rows {
            let mut row = AVec::with_capacity_in(ncols, alloc);
            for cell in line.split(sep as char).take(ncols) {
                row.push(AString::from_str_in(cell.trim().trim_matches('"'), alloc));
            }
            while row.len() < ncols {
                row.push(AString::new_in(alloc));
            }
            rows.push(row);
        }
    }
    let mut sub = AString::new_in(alloc);
    let _ = write!(sub, "{total} rows · {ncols} cols");
    Some(PreviewTable {
        title: AString::from_str_in(label, alloc),
        subtitle: sub,
        headers,
        rows,
        total_rows: total,
    })
}

fn preview_toml<'a>(
    label: &str,
    text: &str,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewBody<'a>> {
    let doc = blaze_toml::Document::parse_in(text, blaze_toml::Limits::SHIPPED, alloc).ok()?;
    let val = doc.to_value_in(alloc);
    preview_data_value(label, &val, limits, alloc)
}

fn preview_json<'a>(
    label: &str,
    text: &str,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewBody<'a>> {
    let val = blaze_json::read_in(text.as_bytes(), blaze_json::Limits::SHIPPED, alloc).ok()?;
    preview_data_value(label, &val, limits, alloc)
}

fn preview_jsonl<'a>(
    text: &str,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewTable<'a>> {
    let mut headers: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    let mut rows: AVec<'a, AVec<'a, AString<'a>>> = AVec::new_in(alloc);
    let mut total = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(DataValue::Map(ref map)) =
            blaze_json::read_in(t.as_bytes(), blaze_json::Limits::SHIPPED, alloc)
        else {
            continue;
        };
        total += 1;
        if headers.is_empty() {
            for (k, _) in map.iter().take(limits.max_cols) {
                headers.push(AString::from_str_in(k, alloc));
            }
        }
        if rows.len() < limits.max_rows && !headers.is_empty() {
            let mut row = AVec::with_capacity_in(headers.len(), alloc);
            for h in headers.iter() {
                let cell = map
                    .iter()
                    .find(|(k, _)| *k == h.as_str())
                    .map(|(_, v)| format_compact_value(v, limits.max_cell_chars, alloc))
                    .unwrap_or_else(|| AString::from_str_in("—", alloc));
                row.push(cell);
            }
            rows.push(row);
        }
    }
    if headers.is_empty() || rows.is_empty() {
        return None;
    }
    let mut sub = AString::new_in(alloc);
    let _ = write!(sub, "{total} records");
    Some(PreviewTable {
        title: AString::from_str_in("JSONL", alloc),
        subtitle: sub,
        headers,
        rows,
        total_rows: total,
    })
}

fn preview_data_value<'a>(
    label: &str,
    val: &DataValue<Alloc<'a>>,
    limits: PreviewLimits,
    alloc: Alloc<'a>,
) -> Option<PreviewBody<'a>> {
    match val {
        DataValue::List(items) => {
            // Array of objects -> crisp tabular view!
            let first_map = match items.first()? {
                DataValue::Map(m) => m,
                _ => return None,
            };
            let mut headers = AVec::new_in(alloc);
            for (k, _) in first_map.iter().take(limits.max_cols) {
                headers.push(AString::from_str_in(k, alloc));
            }
            if headers.is_empty() {
                return None;
            }
            let mut rows = AVec::new_in(alloc);
            for item in items.iter().take(limits.max_rows) {
                if let DataValue::Map(m) = item {
                    let mut row = AVec::with_capacity_in(headers.len(), alloc);
                    for h in headers.iter() {
                        let cell = m
                            .iter()
                            .find(|(k, _)| *k == h.as_str())
                            .map(|(_, v)| format_compact_value(v, limits.max_cell_chars, alloc))
                            .unwrap_or_else(|| AString::from_str_in("—", alloc));
                        row.push(cell);
                    }
                    rows.push(row);
                }
            }
            let mut sub = AString::new_in(alloc);
            let _ = write!(sub, "{} rows", items.len());
            Some(PreviewBody::Table(PreviewTable {
                title: AString::from_str_in(label, alloc),
                subtitle: sub,
                headers,
                rows,
                total_rows: items.len(),
            }))
        }
        DataValue::Map(map) => {
            let mut pairs: AVec<'a, (AString<'a>, AString<'a>)> = AVec::new_in(alloc);
            let mut table: Option<PreviewTable<'a>> = None;

            // Special-case `Cargo.toml` (`[package]` + `[dependencies]`) & `flake.lock` (`nodes`)!
            for (k, v) in map.iter() {
                match (k, v) {
                    ("package" | "project", DataValue::Map(pkg)) => {
                        for (pk, pv) in pkg.iter().take(8) {
                            pairs.push((
                                AString::from_str_in(pk, alloc),
                                format_compact_value(pv, limits.max_cell_chars, alloc),
                            ));
                        }
                    }
                    ("dependencies" | "dev-dependencies" | "nodes", DataValue::Map(deps))
                        if table.is_none() =>
                    {
                        let mut headers = AVec::with_capacity_in(2, alloc);
                        headers.push(AString::from_str_in(if k == "nodes" { "input" } else { "dependency" }, alloc));
                        headers.push(AString::from_str_in("spec", alloc));
                        let mut rows = AVec::new_in(alloc);
                        for (dk, dv) in deps.iter().take(limits.max_rows) {
                            let mut row = AVec::with_capacity_in(2, alloc);
                            row.push(AString::from_str_in(dk, alloc));
                            row.push(format_compact_value(dv, limits.max_cell_chars, alloc));
                            rows.push(row);
                        }
                        let mut sub = AString::new_in(alloc);
                        let _ = write!(sub, "{} entries", deps.len());
                        table = Some(PreviewTable {
                            title: AString::from_str_in(k, alloc),
                            subtitle: sub,
                            headers,
                            rows,
                            total_rows: deps.len(),
                        });
                    }
                    _ => {
                        if pairs.len() < 16 {
                            pairs.push((
                                AString::from_str_in(k, alloc),
                                format_compact_value(v, limits.max_cell_chars, alloc),
                            ));
                        }
                    }
                }
            }
            if pairs.is_empty() && table.is_none() {
                return None;
            }
            let mut sub = AString::new_in(alloc);
            let _ = write!(sub, "{} fields", map.len());
            Some(PreviewBody::KeyValues(PreviewKv {
                title: AString::from_str_in(label, alloc),
                subtitle: sub,
                pairs,
                table,
            }))
        }
        _ => None,
    }
}

fn format_compact_value<'a>(val: &DataValue<Alloc<'a>>, max_chars: usize, alloc: Alloc<'a>) -> AString<'a> {
    let mut s = AString::new_in(alloc);
    match val {
        DataValue::Null => s.push_str("null"),
        DataValue::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
        DataValue::Int(n) => {
            let _ = write!(s, "{}", n.as_i64().unwrap_or(0));
        }
        DataValue::Float(f) => {
            let _ = write!(s, "{f}");
        }
        DataValue::Text(t) => s.push_str(t.as_str()),
        DataValue::Bytes(b) => {
            let _ = write!(s, "<{} bytes>", b.len());
        }
        DataValue::List(xs) => {
            let _ = write!(s, "[{} items]", xs.len());
        }
        DataValue::Map(m) => {
            // Concise summary for Cargo dep specs like `{ path = "../blaze-own" }` or `{ version = "0.6" }`.
            if let Some((_, DataValue::Text(p))) = m.iter().find(|(k, _)| *k == "path") {
                let _ = write!(s, "path: {}", p.as_str());
            } else if let Some((_, DataValue::Text(v))) = m.iter().find(|(k, _)| *k == "version") {
                s.push_str(v.as_str());
            } else if let Some((_, DataValue::Bool(true))) = m.iter().find(|(k, _)| *k == "workspace") {
                s.push_str("workspace");
            } else {
                let _ = write!(s, "{{{} keys}}", m.len());
            }
        }
    }
    if s.chars().count() > max_chars {
        let mut cut = AString::new_in(alloc);
        for ch in s.chars().take(max_chars.saturating_sub(1)) {
            cut.push(ch);
        }
        cut.push('…');
        return cut;
    }
    s
}

#[cfg(test)]
mod tests;
