//! Pure `#![no_std]`, zero-unsafe, sans-IO structural schema & data-profile inference
//! for [`Value`] (`schema` / `describe` builtins).
//!
//! Produces a pipeable [`Value::Record`] tagged with `__blaze_schema = true` containing:
//! - `kind`: root value type (`record`, `table`, `list`, `output`, `int`, …)
//! - `summary`: compact root summary (`record · 2 fields`, `table · 182 rows × 5 cols`, …)
//! - `rows`: primary row/item count (`int`)
//! - `fields`: a [`Value::Table`] with one row per profiled field/column (`path`, `name`,
//!   `depth`, `type`, `pattern`, `present`, `total`, `unique`, `sample`, `hist`).
//!
//! `blaze-shell` detects `__blaze_schema = true` and renders it as an interactive native
//! GPU schema & data-profile card with type/pattern chips, presence fill bars, 8-bin
//! numeric histograms, and clickable `.path` queries.

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc, Rc, Share, Vec};

use crate::persist::PVec;
use crate::value::{Record, Table, Value};

/// Marker key present on records returned by [`infer_schema_value`].
pub const SCHEMA_MARKER_KEY: &str = "__blaze_schema";

/// Hidden O(1) CoW reference to the inspected subject value so running `open Cargo.lock | schema`
/// (or `schema $lock`) binds `$_` to the underlying data rather than the schema meta-card.
pub const SCHEMA_SUBJECT_KEY: &str = "__blaze_schema_subject";

const MAX_DEPTH: usize = 4;
const MAX_FIELDS: usize = 128;
const MAX_SAMPLE_ROWS: usize = 4096;
const MAX_DISTINCT_TRACK: usize = 256;

struct FieldRow<'a> {
    path: AString<'a>,
    name: AString<'a>,
    depth: i64,
    ty: AString<'a>,
    pattern: AString<'a>,
    present: i64,
    total: i64,
    unique: i64,
    sample: AString<'a>,
    hist: AString<'a>,
}

/// Infer a recursive structural schema and statistical data profile for `value`.
pub fn infer_schema_value<'a>(value: &Value<'a>, alloc: Alloc<'a>) -> Value<'a> {
    let mut rows: AVec<'a, FieldRow<'a>> = Vec::new_in(alloc);
    let kind_str = value.type_name();
    let mut summary = AString::new_in(alloc);
    let root_count: i64;

    match value {
        Value::Record(rec) => {
            root_count = rec.len() as i64;
            let _ = write!(
                summary,
                "record · {} {}",
                rec.len(),
                if rec.len() == 1 { "field" } else { "fields" }
            );
            profile_record_fields(rec, "", 0, &mut rows, alloc);
        }
        Value::Table(tbl) => {
            root_count = tbl.nrows() as i64;
            let _ = write!(
                summary,
                "table · {} {} × {} {}",
                tbl.nrows(),
                if tbl.nrows() == 1 { "row" } else { "rows" },
                tbl.ncols(),
                if tbl.ncols() == 1 { "col" } else { "cols" }
            );
            profile_table_columns(tbl, "", 0, &mut rows, alloc);
        }
        Value::List(items) => {
            root_count = items.len() as i64;
            let _ = write!(
                summary,
                "list · {} {}",
                items.len(),
                if items.len() == 1 { "item" } else { "items" }
            );
            profile_list_root(items, "", 0, &mut rows, alloc);
        }
        Value::Output(out) => {
            root_count = 5;
            let _ = write!(
                summary,
                "output · exit {} · {} B stdout",
                out.exit_code,
                out.stdout_len()
            );
            profile_output_fields(out, "", 0, &mut rows, alloc);
        }
        scalar => {
            root_count = 1;
            let mut val_preview = AString::new_in(alloc);
            format_compact_value(scalar, 64, &mut val_preview);
            let _ = write!(summary, "{} · {}", kind_str, val_preview.as_str());
            let mut pat = AString::new_in(alloc);
            if let Value::String(s) = scalar
                && let Some(fmt) = detect_string_format(s.as_ref())
            {
                pat.push_str(fmt);
            }
            rows.push(FieldRow {
                path: AString::from_str_in(".", alloc),
                name: AString::from_str_in(".", alloc),
                depth: 0,
                ty: AString::from_str_in(kind_str, alloc),
                pattern: pat,
                present: i64::from(!matches!(scalar, Value::Nothing)),
                total: 1,
                unique: i64::from(!matches!(scalar, Value::Nothing)),
                sample: val_preview,
                hist: AString::new_in(alloc),
            });
        }
    }

    let mut cols = PVec::new(alloc);
    for name in [
        "path", "name", "depth", "type", "pattern", "present", "total", "unique", "sample", "hist",
    ] {
        cols.push(Rc::from_str_in(name, alloc));
    }
    let mut table = Table::new(cols, alloc);
    for r in rows.iter() {
        let vals = [
            Value::string(r.path.as_str(), alloc),
            Value::string(r.name.as_str(), alloc),
            Value::Int(r.depth),
            Value::string(r.ty.as_str(), alloc),
            Value::string(r.pattern.as_str(), alloc),
            Value::Int(r.present),
            Value::Int(r.total),
            Value::Int(r.unique),
            Value::string(r.sample.as_str(), alloc),
            Value::string(r.hist.as_str(), alloc),
        ];
        table.push_row_values(&vals);
    }

    let mut out = Record::new(alloc);
    out.insert(Rc::from_str_in(SCHEMA_MARKER_KEY, alloc), Value::Bool(true));
    out.insert(Rc::from_str_in(SCHEMA_SUBJECT_KEY, alloc), value.share());
    out.insert(Rc::from_str_in("kind", alloc), Value::string(kind_str, alloc));
    out.insert(Rc::from_str_in("summary", alloc), Value::string(summary.as_str(), alloc));
    out.insert(Rc::from_str_in("rows", alloc), Value::Int(root_count));
    out.insert(Rc::from_str_in("fields", alloc), Value::Table(table));
    Value::Record(out)
}

fn push_child_path<'a>(parent: &str, key: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut p = AString::with_capacity_in(parent.len() + key.len() + 2, alloc);
    p.push_str(parent);
    p.push('.');
    p.push_str(key);
    p
}

fn profile_record_fields<'a>(
    rec: &Record<'a>,
    parent_path: &str,
    depth: usize,
    rows: &mut AVec<'a, FieldRow<'a>>,
    alloc: Alloc<'a>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    for (k, v) in rec.iter() {
        if rows.len() >= MAX_FIELDS {
            break;
        }
        let path = push_child_path(parent_path, k, alloc);
        match v {
            Value::Record(sub) => {
                let mut ty = AString::new_in(alloc);
                let _ = write!(ty, "record{{{}}}", sub.len());
                let mut sample = AString::new_in(alloc);
                for (i, (sk, _)) in sub.iter().take(4).enumerate() {
                    if i > 0 {
                        sample.push_str(", ");
                    }
                    sample.push_str(sk);
                }
                if sub.len() > 4 {
                    let _ = write!(sample, ", +{}", sub.len() - 4);
                }
                rows.push(FieldRow {
                    path: AString::from_str_in(path.as_str(), alloc),
                    name: AString::from_str_in(k, alloc),
                    depth: depth as i64,
                    ty,
                    pattern: AString::new_in(alloc),
                    present: 1,
                    total: 1,
                    unique: 1,
                    sample,
                    hist: AString::new_in(alloc),
                });
                profile_record_fields(sub, path.as_str(), depth + 1, rows, alloc);
            }
            Value::Table(tbl) => {
                let mut ty = AString::new_in(alloc);
                let _ = write!(ty, "table[{}]", tbl.nrows());
                let mut sample = AString::new_in(alloc);
                let _ = write!(sample, "{} rows × {} cols", tbl.nrows(), tbl.ncols());
                rows.push(FieldRow {
                    path: AString::from_str_in(path.as_str(), alloc),
                    name: AString::from_str_in(k, alloc),
                    depth: depth as i64,
                    ty,
                    pattern: AString::new_in(alloc),
                    present: 1,
                    total: 1,
                    unique: tbl.nrows() as i64,
                    sample,
                    hist: AString::new_in(alloc),
                });
                profile_table_columns(tbl, path.as_str(), depth + 1, rows, alloc);
            }
            Value::List(items) => {
                let is_record_list = !items.is_empty()
                    && items
                        .iter()
                        .take(32)
                        .all(|it| matches!(it, Value::Record(_) | Value::Nothing))
                    && items.iter().take(32).any(|it| matches!(it, Value::Record(_)));
                if is_record_list {
                    let mut ty = AString::new_in(alloc);
                    let _ = write!(ty, "list<record>[{}]", items.len());
                    let mut sample = AString::new_in(alloc);
                    let _ = write!(sample, "{} records", items.len());
                    rows.push(FieldRow {
                        path: AString::from_str_in(path.as_str(), alloc),
                        name: AString::from_str_in(k, alloc),
                        depth: depth as i64,
                        ty,
                        pattern: AString::new_in(alloc),
                        present: 1,
                        total: 1,
                        unique: items.len() as i64,
                        sample,
                        hist: AString::new_in(alloc),
                    });
                    profile_list_of_records(items, path.as_str(), depth + 1, rows, alloc);
                } else {
                    let mut ty = AString::new_in(alloc);
                    let elem_ty = infer_list_elem_type(items);
                    let _ = write!(ty, "list<{elem_ty}>[{}]", items.len());
                    let mut sample = AString::new_in(alloc);
                    format_compact_value(v, 52, &mut sample);
                    rows.push(FieldRow {
                        path,
                        name: AString::from_str_in(k, alloc),
                        depth: depth as i64,
                        ty,
                        pattern: AString::new_in(alloc),
                        present: 1,
                        total: 1,
                        unique: 1,
                        sample,
                        hist: AString::new_in(alloc),
                    });
                }
            }
            Value::Output(out) => {
                let mut sample = AString::new_in(alloc);
                let _ = write!(sample, "exit {} · {} B stdout", out.exit_code, out.stdout_len());
                rows.push(FieldRow {
                    path: AString::from_str_in(path.as_str(), alloc),
                    name: AString::from_str_in(k, alloc),
                    depth: depth as i64,
                    ty: AString::from_str_in("output", alloc),
                    pattern: AString::new_in(alloc),
                    present: 1,
                    total: 1,
                    unique: 1,
                    sample,
                    hist: AString::new_in(alloc),
                });
                profile_output_fields(out, path.as_str(), depth + 1, rows, alloc);
            }
            scalar => {
                let mut sample = AString::new_in(alloc);
                let mut pat = AString::new_in(alloc);
                if matches!(scalar, Value::Nothing) {
                    sample.push_str("null");
                } else {
                    sample.push_str("= ");
                    format_compact_value(scalar, 52, &mut sample);
                    if let Value::String(s) = scalar {
                        if let Some(fmt) = detect_string_format(s.as_ref()) {
                            pat.push_str(fmt);
                        }
                    } else {
                        pat.push_str("const");
                    }
                }
                rows.push(FieldRow {
                    path,
                    name: AString::from_str_in(k, alloc),
                    depth: depth as i64,
                    ty: AString::from_str_in(scalar.type_name(), alloc),
                    pattern: pat,
                    present: i64::from(!matches!(scalar, Value::Nothing)),
                    total: 1,
                    unique: i64::from(!matches!(scalar, Value::Nothing)),
                    sample,
                    hist: AString::new_in(alloc),
                });
            }
        }
    }
}

fn profile_table_columns<'a>(
    tbl: &Table<'a>,
    parent_path: &str,
    depth: usize,
    rows: &mut AVec<'a, FieldRow<'a>>,
    alloc: Alloc<'a>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let total = tbl.nrows();
    let scan_n = total.min(MAX_SAMPLE_ROWS);
    for ci in 0..tbl.ncols() {
        if rows.len() >= MAX_FIELDS {
            break;
        }
        let col_name = tbl.columns.get(ci).unwrap().as_ref();
        let path = push_child_path(parent_path, col_name, alloc);
        let Some(col) = tbl.cols.get(ci) else {
            continue;
        };
        let mut samples: AVec<'a, Value<'a>> = Vec::with_capacity_in(scan_n.min(256), alloc);
        for ri in 0..scan_n {
            if let Some(v) = col.get(ri) {
                samples.push(v.share());
            }
        }
        let row = profile_value_series(
            path.as_str(),
            col_name,
            depth,
            samples.as_slice(),
            total,
            alloc,
        );
        rows.push(row);

        // If the column contains nested records, also profile one level deeper!
        if depth + 1 < MAX_DEPTH && rows.len() < MAX_FIELDS {
            let mut rec_list = PVec::new(alloc);
            let mut any_rec = false;
            for v in samples.iter().take(256) {
                if matches!(v, Value::Record(_)) {
                    any_rec = true;
                    rec_list.push(v.share());
                }
            }
            if any_rec {
                profile_list_of_records(&rec_list, path.as_str(), depth + 1, rows, alloc);
            }
        }
    }
}

fn profile_list_root<'a>(
    items: &PVec<'a, Value<'a>>,
    parent_path: &str,
    depth: usize,
    rows: &mut AVec<'a, FieldRow<'a>>,
    alloc: Alloc<'a>,
) {
    let is_record_list = !items.is_empty()
        && items
            .iter()
            .take(64)
            .all(|it| matches!(it, Value::Record(_) | Value::Nothing))
        && items.iter().take(64).any(|it| matches!(it, Value::Record(_)));
    if is_record_list {
        profile_list_of_records(items, parent_path, depth, rows, alloc);
        return;
    }
    let total = items.len();
    let scan_n = total.min(MAX_SAMPLE_ROWS);
    let mut samples: AVec<'a, Value<'a>> = Vec::with_capacity_in(scan_n.min(256), alloc);
    for i in 0..scan_n {
        if let Some(v) = items.get(i) {
            samples.push(v.share());
        }
    }
    rows.push(profile_value_series(".*", "items", depth, samples.as_slice(), total, alloc));
}

fn profile_list_of_records<'a>(
    items: &PVec<'a, Value<'a>>,
    parent_path: &str,
    depth: usize,
    rows: &mut AVec<'a, FieldRow<'a>>,
    alloc: Alloc<'a>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let total = items.len();
    let scan_n = total.min(MAX_SAMPLE_ROWS);
    let mut keys: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for i in 0..scan_n {
        if let Some(Value::Record(r)) = items.get(i) {
            for (k, _) in r.iter() {
                if !keys.iter().any(|existing| existing.as_str() == k) {
                    keys.push(AString::from_str_in(k, alloc));
                    if keys.len() >= 48 {
                        break;
                    }
                }
            }
        }
    }
    for k in keys.iter() {
        if rows.len() >= MAX_FIELDS {
            break;
        }
        let path = push_child_path(parent_path, k.as_str(), alloc);
        let mut samples: AVec<'a, Value<'a>> = Vec::with_capacity_in(scan_n.min(256), alloc);
        for i in 0..scan_n {
            let v = match items.get(i) {
                Some(Value::Record(r)) => r.get(k.as_str()).map(Share::share).unwrap_or(Value::Nothing),
                _ => Value::Nothing,
            };
            samples.push(v);
        }
        rows.push(profile_value_series(
            path.as_str(),
            k.as_str(),
            depth,
            samples.as_slice(),
            total,
            alloc,
        ));
    }
}

fn profile_output_fields<'a>(
    out: &crate::value::CommandOutput<'a>,
    parent_path: &str,
    depth: usize,
    rows: &mut AVec<'a, FieldRow<'a>>,
    alloc: Alloc<'a>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let mut exit_s = AString::new_in(alloc);
    let _ = write!(exit_s, "= {}", out.exit_code);
    rows.push(FieldRow {
        path: push_child_path(parent_path, "exit_code", alloc),
        name: AString::from_str_in("exit_code", alloc),
        depth: depth as i64,
        ty: AString::from_str_in("int", alloc),
        pattern: AString::from_str_in("const", alloc),
        present: 1,
        total: 1,
        unique: 1,
        sample: exit_s,
        hist: AString::new_in(alloc),
    });
    let mut dur_s = AString::new_in(alloc);
    let _ = Value::Duration(out.duration).format_cell(&mut dur_s);
    rows.push(FieldRow {
        path: push_child_path(parent_path, "duration", alloc),
        name: AString::from_str_in("duration", alloc),
        depth: depth as i64,
        ty: AString::from_str_in("duration", alloc),
        pattern: AString::new_in(alloc),
        present: 1,
        total: 1,
        unique: 1,
        sample: dur_s,
        hist: AString::new_in(alloc),
    });
    let mut out_s = AString::new_in(alloc);
    let _ = write!(out_s, "{} bytes", out.stdout_len());
    rows.push(FieldRow {
        path: push_child_path(parent_path, "stdout", alloc),
        name: AString::from_str_in("stdout", alloc),
        depth: depth as i64,
        ty: AString::from_str_in("string", alloc),
        pattern: AString::new_in(alloc),
        present: i64::from(out.stdout_len() > 0),
        total: 1,
        unique: 1,
        sample: out_s,
        hist: AString::new_in(alloc),
    });
    let mut err_s = AString::new_in(alloc);
    let _ = write!(err_s, "{} bytes", out.stderr_len());
    rows.push(FieldRow {
        path: push_child_path(parent_path, "stderr", alloc),
        name: AString::from_str_in("stderr", alloc),
        depth: depth as i64,
        ty: AString::from_str_in("string", alloc),
        pattern: AString::new_in(alloc),
        present: i64::from(out.stderr_len() > 0),
        total: 1,
        unique: 1,
        sample: err_s,
        hist: AString::new_in(alloc),
    });
}

fn profile_value_series<'a>(
    path: &str,
    name: &str,
    depth: usize,
    samples: &[Value<'a>],
    total: usize,
    alloc: Alloc<'a>,
) -> FieldRow<'a> {
    let mut present = 0usize;
    let mut primary_ty: Option<&'static str> = None;
    let mut mixed_types = false;
    let mut list_elem_ty: Option<&'static str> = None;
    let mut list_min_len = usize::MAX;
    let mut list_max_len = 0usize;
    let mut list_sum_len = 0usize;
    let mut list_count = 0usize;

    // Numeric tracking (int, float, size, duration)
    let mut nums: AVec<'a, f64> = Vec::new_in(alloc);
    let mut num_min = f64::INFINITY;
    let mut num_max = f64::NEG_INFINITY;
    let mut num_sum = 0.0f64;

    // Distinct value frequency tracking (bounded cleanly)
    let mut distinct: AVec<'a, (AString<'a>, u32)> = Vec::new_in(alloc);
    let mut distinct_overflow = false;

    // String format counters & common prefix
    let mut str_count = 0usize;
    let mut fmt_semver = 0usize;
    let mut fmt_url = 0usize;
    let mut fmt_hex = 0usize;
    let mut hex_len = 0usize;
    let mut hex_uniform = true;
    let mut fmt_uuid = 0usize;
    let mut fmt_date = 0usize;
    let mut fmt_email = 0usize;
    let mut fmt_path = 0usize;
    let mut common_prefix: Option<AString<'a>> = None;
    let mut first_list_sample: Option<AString<'a>> = None;

    for v in samples {
        if matches!(v, Value::Nothing) {
            continue;
        }
        present += 1;
        let tn = v.type_name();
        match primary_ty {
            None => primary_ty = Some(tn),
            Some(prev) if prev == tn => {}
            Some("int") if tn == "float" => primary_ty = Some("float"),
            Some("float") if tn == "int" => {}
            _ => mixed_types = true,
        }

        match v {
            Value::Int(n) => {
                let f = *n as f64;
                nums.push(f);
                num_min = num_min.min(f);
                num_max = num_max.max(f);
                num_sum += f;
            }
            Value::Float(f) if f.is_finite() => {
                nums.push(*f);
                num_min = num_min.min(*f);
                num_max = num_max.max(*f);
                num_sum += *f;
            }
            Value::Size(b) => {
                let f = b.0 as f64;
                nums.push(f);
                num_min = num_min.min(f);
                num_max = num_max.max(f);
                num_sum += f;
            }
            Value::Duration(d) => {
                let f = d.0 as f64;
                nums.push(f);
                num_min = num_min.min(f);
                num_max = num_max.max(f);
                num_sum += f;
            }
            Value::String(s) => {
                let st = s.as_ref();
                str_count += 1;
                if is_semver(st) {
                    fmt_semver += 1;
                } else if is_uuid(st) {
                    fmt_uuid += 1;
                } else if is_url_like(st) {
                    fmt_url += 1;
                } else if is_hex_hash(st) {
                    fmt_hex += 1;
                    if hex_len == 0 {
                        hex_len = st.len();
                    } else if hex_len != st.len() {
                        hex_uniform = false;
                    }
                } else if is_iso_date(st) {
                    fmt_date += 1;
                } else if is_email(st) {
                    fmt_email += 1;
                } else if is_path_like(st) {
                    fmt_path += 1;
                }
                if !st.is_empty() {
                    match &mut common_prefix {
                        None => common_prefix = Some(AString::from_str_in(st, alloc)),
                        Some(cp) => {
                            let new_len = common_char_prefix_len(cp.as_str(), st);
                            cp.truncate(new_len);
                        }
                    }
                }
            }
            Value::List(items) => {
                list_count += 1;
                let len = items.len();
                list_min_len = list_min_len.min(len);
                list_max_len = list_max_len.max(len);
                list_sum_len += len;
                let lf = len as f64;
                nums.push(lf);
                num_min = num_min.min(lf);
                num_max = num_max.max(lf);
                num_sum += lf;
                if list_elem_ty.is_none() && !items.is_empty() {
                    list_elem_ty = Some(infer_list_elem_type(items));
                }
                if first_list_sample.is_none() && !items.is_empty() {
                    let mut ls = AString::new_in(alloc);
                    format_compact_value(v, 44, &mut ls);
                    first_list_sample = Some(ls);
                }
            }
            _ => {}
        }

        // Track distinct formatted values (for non-list / non-record values)
        if !matches!(v, Value::List(_) | Value::Record(_) | Value::Table(_)) {
            let mut key = AString::new_in(alloc);
            format_compact_value(v, 48, &mut key);
            if let Some((_, count)) = distinct.iter_mut().find(|(d, _)| d.as_str() == key.as_str()) {
                *count = count.saturating_add(1);
            } else if distinct.len() < MAX_DISTINCT_TRACK {
                distinct.push((key, 1));
            } else {
                distinct_overflow = true;
            }
        }
    }

    // Scale `present` proportionally if `total > samples.len()`.
    let scaled_present = if samples.len() == total || samples.is_empty() {
        present
    } else {
        (present * total) / samples.len()
    };
    let nullable = present < samples.len();

    // Build readable type label (`string?`, `list<string>?`, `int`, `mixed`)
    let mut ty = AString::new_in(alloc);
    if present == 0 {
        ty.push_str("null");
    } else if mixed_types {
        ty.push_str("mixed");
    } else if primary_ty == Some("list") {
        let et = list_elem_ty.unwrap_or("any");
        let _ = write!(ty, "list<{et}>");
    } else {
        ty.push_str(primary_ty.unwrap_or("any"));
    }
    if nullable && present > 0 {
        ty.push('?');
    }

    // Sort distinct entries descending by frequency for top-value reporting.
    distinct.as_mut_slice().sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));

    // Infer semantic pattern label
    let mut pattern = AString::new_in(alloc);
    let unique_count = if distinct_overflow {
        present
    } else {
        distinct.len()
    };
    let threshold = (str_count * 7 + 9) / 10; // >= 70%
    if present > 0 && !distinct_overflow && distinct.len() == 1 && present >= 2 {
        if str_count > 0 && fmt_url >= threshold {
            pattern.push_str("const url");
        } else if str_count > 0 && fmt_semver >= threshold {
            pattern.push_str("x.y.z");
        } else {
            pattern.push_str("const");
        }
    } else if str_count > 0 && fmt_semver >= threshold {
        pattern.push_str("x.y.z");
    } else if str_count > 0 && fmt_uuid >= threshold {
        pattern.push_str("uuid");
    } else if str_count > 0 && fmt_hex >= threshold {
        if hex_uniform && hex_len > 0 {
            let _ = write!(pattern, "hex-{hex_len}");
        } else {
            pattern.push_str("hex");
        }
    } else if str_count > 0 && fmt_url >= threshold {
        if let Some(cp) = &common_prefix
            && cp.len() >= 10
        {
            let short = truncate_prefix_display(cp.as_str(), 22);
            let _ = write!(pattern, "{short}...");
        } else {
            pattern.push_str("url");
        }
    } else if str_count > 0 && fmt_date >= threshold {
        pattern.push_str("iso-date");
    } else if str_count > 0 && fmt_email >= threshold {
        pattern.push_str("email");
    } else if str_count > 0 && fmt_path >= threshold {
        pattern.push_str("path");
    } else if str_count >= 3
        && let Some(cp) = &common_prefix
        && cp.len() >= 4
    {
        let short = truncate_prefix_display(cp.as_str(), 20);
        let _ = write!(pattern, "{short}...");
    } else if primary_ty == Some("list") && list_count > 0 {
        let _ = write!(pattern, "{list_min_len}..{list_max_len}");
    } else if present >= 3
        && !distinct_overflow
        && !distinct.is_empty()
        && distinct.len() <= 6
        && !matches!(primary_ty, Some("bool"))
    {
        pattern.push_str("enum");
    } else if primary_ty == Some("bool") && present > 0 {
        pattern.push_str("bool");
    } else if !distinct_overflow && distinct.len() == present && present >= 4 {
        pattern.push_str("unique");
    }

    // Build 8-bin histogram string for numeric/size/duration OR categorical top-K bars.
    let mut hist = AString::new_in(alloc);
    if !nums.is_empty() && num_max > num_min {
        let mut bins = [0u32; 8];
        let span = num_max - num_min;
        for &v in nums.iter() {
            let norm = ((v - num_min) / span).clamp(0.0, 1.0);
            let idx = ((norm * 8.0) as usize).min(7);
            bins[idx] = bins[idx].saturating_add(1);
        }
        hist.push_str("num:");
        for (i, &b) in bins.iter().enumerate() {
            if i > 0 {
                hist.push(',');
            }
            let _ = write!(hist, "{b}");
        }
    } else if !distinct.is_empty() && present > 0 {
        // Encode up to 5 top-value shares (`cat:c0,c1,c2,...`) for proportional stacked bar!
        hist.push_str("cat:");
        let take_n = distinct.len().min(5);
        for (i, (_, c)) in distinct.iter().take(take_n).enumerate() {
            if i > 0 {
                hist.push(',');
            }
            let _ = write!(hist, "{c}");
        }
        let top_sum: u32 = distinct.iter().take(take_n).map(|(_, c)| *c).sum();
        let rest = (present as u32).saturating_sub(top_sum);
        if rest > 0 {
            let _ = write!(hist, ",{rest}");
        }
    }

    // Build crisp, scannable sample / summary string.
    let mut sample = AString::new_in(alloc);
    if present == 0 {
        sample.push_str("all null");
    } else if !nums.is_empty() && matches!(primary_ty, Some("int" | "float" | "size" | "duration")) {
        let mean = num_sum / (nums.len() as f64);
        match primary_ty {
            Some("size") => {
                let _ = Value::Size(crate::value::Bytes(num_min as i64)).format_cell(&mut sample);
                if num_max > num_min {
                    sample.push_str(" .. ");
                    let _ = Value::Size(crate::value::Bytes(num_max as i64)).format_cell(&mut sample);
                    sample.push_str(" · avg ");
                    let _ = Value::Size(crate::value::Bytes(mean as i64)).format_cell(&mut sample);
                }
            }
            Some("duration") => {
                let _ = Value::Duration(crate::value::Nanos(num_min as i64)).format_cell(&mut sample);
                if num_max > num_min {
                    sample.push_str(" .. ");
                    let _ = Value::Duration(crate::value::Nanos(num_max as i64)).format_cell(&mut sample);
                }
            }
            Some("int") => {
                if (num_max - num_min).abs() < f64::EPSILON {
                    let _ = write!(sample, "= {}", num_min as i64);
                } else {
                    let _ = write!(sample, "{} .. {} · avg {:.1}", num_min as i64, num_max as i64, mean);
                }
            }
            _ => {
                if (num_max - num_min).abs() < 1e-9 {
                    let _ = write!(sample, "= {:.2}", num_min);
                } else {
                    let _ = write!(sample, "{:.2} .. {:.2} · avg {:.2}", num_min, num_max, mean);
                }
            }
        }
    } else if primary_ty == Some("list") && list_count > 0 {
        let avg = list_sum_len as f64 / list_count as f64;
        let _ = write!(sample, "{list_min_len}..{list_max_len} items · avg {avg:.1}");
        if let Some(ex) = first_list_sample {
            sample.push_str(" · ");
            sample.push_str(ex.as_str());
        }
    } else if !distinct.is_empty() {
        if !distinct_overflow && distinct.len() == 1 {
            sample.push_str("= ");
            sample.push_str(distinct[0].0.as_str());
        } else if !distinct_overflow && distinct.len() <= 4 {
            for (i, (val, cnt)) in distinct.iter().enumerate() {
                if i > 0 {
                    sample.push_str(", ");
                }
                sample.push_str(val.as_str());
                if *cnt > 1 {
                    let _ = write!(sample, " ({cnt})");
                }
            }
        } else {
            // Show distinct count + top/representative examples
            if distinct_overflow {
                let _ = write!(sample, "{present}+ unique · ");
            } else {
                let _ = write!(sample, "{} unique · ", distinct.len());
            }
            for (i, (val, cnt)) in distinct.iter().take(3).enumerate() {
                if i > 0 {
                    sample.push_str(", ");
                }
                sample.push_str(val.as_str());
                if *cnt > 1 && distinct.len() < present {
                    let _ = write!(sample, " ({cnt})");
                }
            }
        }
    }

    FieldRow {
        path: AString::from_str_in(path, alloc),
        name: AString::from_str_in(name, alloc),
        depth: depth as i64,
        ty,
        pattern,
        present: scaled_present as i64,
        total: total as i64,
        unique: unique_count as i64,
        sample,
        hist,
    }
}

fn infer_list_elem_type<'a>(items: &PVec<'a, Value<'a>>) -> &'static str {
    let mut ty: Option<&'static str> = None;
    for v in items.iter().take(32) {
        if matches!(v, Value::Nothing) {
            continue;
        }
        let tn = v.type_name();
        match ty {
            None => ty = Some(tn),
            Some(prev) if prev == tn => {}
            _ => return "any",
        }
    }
    ty.unwrap_or("any")
}

fn format_compact_value<'a>(v: &Value<'a>, max_chars: usize, out: &mut AString<'a>) {
    match v {
        Value::List(items) => {
            out.push('[');
            for (i, it) in items.iter().take(3).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = it.format_cell(out);
                if out.len() >= max_chars {
                    break;
                }
            }
            if items.len() > 3 {
                let _ = write!(out, ", +{}", items.len() - 3);
            }
            out.push(']');
        }
        _ => {
            let _ = v.format_cell(out);
        }
    }
    if out.chars().count() > max_chars {
        let cut = out
            .char_indices()
            .nth(max_chars.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(out.len());
        out.truncate(cut);
        out.push('…');
    }
}

fn detect_string_format(s: &str) -> Option<&'static str> {
    if is_semver(s) {
        Some("x.y.z")
    } else if is_uuid(s) {
        Some("uuid")
    } else if is_url_like(s) {
        Some("url")
    } else if is_hex_hash(s) {
        Some("hex")
    } else if is_iso_date(s) {
        Some("iso-date")
    } else if is_email(s) {
        Some("email")
    } else if is_path_like(s) {
        Some("path")
    } else {
        None
    }
}

fn is_semver(s: &str) -> bool {
    let s = s.strip_prefix('v').unwrap_or(s);
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let (Some(a), Some(b), Some(c), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && !c.is_empty()
        && a.bytes().all(|x| x.is_ascii_digit())
        && b.bytes().all(|x| x.is_ascii_digit())
        && c.bytes().all(|x| x.is_ascii_digit())
}

fn is_url_like(s: &str) -> bool {
    s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("registry+https://")
        || s.starts_with("git+https://")
        || s.starts_with("ssh://")
        || s.starts_with("file://")
}

fn is_hex_hash(s: &str) -> bool {
    s.len() >= 8
        && s.bytes().all(|b| b.is_ascii_hexdigit())
        && s.bytes().any(|b| b.is_ascii_alphabetic())
        && s.bytes().any(|b| b.is_ascii_digit())
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b[8] == b'-'
        && b[13] == b'-'
        && b[18] == b'-'
        && b[23] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, &c)| matches!(i, 8 | 13 | 18 | 23) || c.is_ascii_hexdigit())
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn is_email(s: &str) -> bool {
    let Some((user, dom)) = s.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !user.contains(' ')
        && dom.contains('.')
        && !dom.starts_with('.')
        && !dom.ends_with('.')
        && !dom.contains(' ')
}

fn is_path_like(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with("~/")
}

fn common_char_prefix_len(a: &str, b: &str) -> usize {
    let mut last = 0usize;
    for ((ia, ca), (_, cb)) in a.char_indices().zip(b.char_indices()) {
        if ca != cb {
            break;
        }
        last = ia + ca.len_utf8();
    }
    last
}

fn truncate_prefix_display(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    s.char_indices()
        .nth(max_chars)
        .map(|(i, _)| &s[..i])
        .unwrap_or(s)
}
