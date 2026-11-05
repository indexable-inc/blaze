//! The reports `pprof` prints, on a [`View`]: `top`, `peek`, `tree`, `list`, folded stacks, a
//! text flame graph and `diff`. Each takes the index of the sample type to weigh by and gives
//! text whose order follows from the data alone (ties broken by name).

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use regex::Regex;

use crate::view::{Sample, View};

/// One value in its unit: `ns` as time, `bytes` as sizes, the rest as counts.
pub fn format_value(value: i64, unit: &str) -> String {
    let v = value as f64;
    let neg = if value < 0 { "-" } else { "" };
    let a = v.abs();
    match unit {
        "nanoseconds" | "ns" => {
            if a >= 1e9 {
                format!("{neg}{:.2}s", a / 1e9)
            } else if a >= 1e6 {
                format!("{neg}{:.2}ms", a / 1e6)
            } else if a >= 1e3 {
                format!("{neg}{:.2}us", a / 1e3)
            } else {
                format!("{value}ns")
            }
        }
        "bytes" => {
            if a >= (1u64 << 30) as f64 {
                format!("{neg}{:.2}GiB", a / (1u64 << 30) as f64)
            } else if a >= (1u64 << 20) as f64 {
                format!("{neg}{:.2}MiB", a / (1u64 << 20) as f64)
            } else if a >= 1024.0 {
                format!("{neg}{:.2}KiB", a / 1024.0)
            } else {
                format!("{value}B")
            }
        }
        _ => format!("{value}"),
    }
}

fn pct(part: i64, total: i64) -> String {
    if total == 0 {
        "0.00%".to_owned()
    } else {
        format!("{:.2}%", part as f64 * 100.0 / total as f64)
    }
}

fn value(s: &Sample, ty: usize) -> i64 {
    s.values.get(ty).copied().unwrap_or(0)
}

fn unit(view: &View, ty: usize) -> &str {
    view.sample_types.get(ty).map_or("count", |it| it.unit.as_str())
}

/// The total of sample type `ty`, of absolute values (a diff has negative samples).
pub fn total(view: &View, ty: usize) -> i64 {
    view.samples.iter().map(|s| value(s, ty).abs()).sum()
}

/// Flat (the function is the leaf) and cumulative (anywhere on the stack, once a sample)
/// values by function name.
fn flat_cum(view: &View, ty: usize) -> BTreeMap<&str, (i64, i64)> {
    let mut by: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for s in &view.samples {
        let v = value(s, ty);
        let names = view.names(s);
        if let Some(leaf) = names.last() {
            by.entry(leaf).or_default().0 += v;
        }
        let mut seen: Vec<&str> = names.clone();
        seen.sort_unstable();
        seen.dedup();
        for n in seen {
            by.entry(n).or_default().1 += v;
        }
    }
    by
}

/// `top`: the `n` functions with the most flat (or, `cum`, cumulative) value.
pub fn top(view: &View, ty: usize, cum: bool, n: usize) -> String {
    let u = unit(view, ty);
    let total = total(view, ty);
    let mut rows: Vec<(&str, i64, i64)> =
        flat_cum(view, ty).into_iter().map(|(k, (f, c))| (k, f, c)).collect();
    rows.sort_by(|a, b| {
        let key = |r: &(&str, i64, i64)| if cum { r.2.abs() } else { r.1.abs() };
        key(b).cmp(&key(a)).then(a.0.cmp(b.0))
    });
    let mut out = String::new();
    let ty_name = view.sample_types.get(ty).map_or("?", |it| it.name.as_str());
    let _ = writeln!(
        out,
        "{ty_name}: total {} in {} samples",
        format_value(total, u),
        view.samples.len()
    );
    let _ = writeln!(
        out,
        "{:>12} {:>7} {:>7} {:>12} {:>7}  function",
        "flat", "flat%", "sum%", "cum", "cum%"
    );
    let mut sum = 0;
    for (name, flat, cumv) in rows.into_iter().take(n) {
        sum += flat;
        let _ = writeln!(
            out,
            "{:>12} {:>7} {:>7} {:>12} {:>7}  {name}",
            format_value(flat, u),
            pct(flat, total),
            pct(sum, total),
            format_value(cumv, u),
            pct(cumv, total),
        );
    }
    out
}

/// `peek <regex>`: for each matching function, who calls it and what it calls, by value.
pub fn peek(view: &View, ty: usize, pattern: &Regex) -> String {
    let u = unit(view, ty);
    let total = total(view, ty);
    let mut callers: BTreeMap<&str, BTreeMap<&str, i64>> = BTreeMap::new();
    let mut callees: BTreeMap<&str, BTreeMap<&str, i64>> = BTreeMap::new();
    let mut flat: BTreeMap<&str, i64> = BTreeMap::new();
    let mut cum: BTreeMap<&str, i64> = BTreeMap::new();
    for s in &view.samples {
        let v = value(s, ty);
        let names = view.names(s);
        let mut counted: Vec<&str> = Vec::new();
        for (i, n) in names.iter().enumerate() {
            if !pattern.is_match(n) || counted.contains(n) {
                continue;
            }
            counted.push(n);
            *cum.entry(n).or_default() += v;
            *callers
                .entry(n)
                .or_default()
                .entry(if i == 0 { "(root)" } else { names[i - 1] })
                .or_default() += v;
            match names.get(i + 1) {
                Some(next) => *callees.entry(n).or_default().entry(next).or_default() += v,
                None => *flat.entry(n).or_default() += v,
            }
        }
    }
    let mut out = String::new();
    let sorted = |m: &BTreeMap<&str, i64>| {
        let mut v: Vec<(String, i64)> = m.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        v.sort_by(|a, b| b.1.abs().cmp(&a.1.abs()).then(a.0.cmp(&b.0)));
        v
    };
    for (name, c) in &cum {
        let _ = writeln!(
            out,
            "{name}: cum {} ({}), flat {}",
            format_value(*c, u),
            pct(*c, total),
            format_value(flat.get(name).copied().unwrap_or(0), u)
        );
        for (caller, v) in sorted(callers.get(name).unwrap_or(&BTreeMap::new())) {
            let _ = writeln!(out, "  {:>12} {:>7}  <- {caller}", format_value(v, u), pct(v, *c));
        }
        for (callee, v) in sorted(callees.get(name).unwrap_or(&BTreeMap::new())) {
            let _ = writeln!(out, "  {:>12} {:>7}  -> {callee}", format_value(v, u), pct(v, *c));
        }
    }
    if cum.is_empty() {
        let _ = writeln!(out, "no function matches {pattern}");
    }
    out
}

/// A call tree from the roots, by name path; nodes under `min_fraction` of the total are cut.
#[derive(Default)]
struct Node<'a> {
    total: i64,
    children: BTreeMap<&'a str, Node<'a>>,
}

fn build<'a>(view: &'a View, ty: usize, reverse: bool) -> Node<'a> {
    let mut root = Node::default();
    for s in &view.samples {
        let v = value(s, ty);
        let mut names = view.names(s);
        if reverse {
            names.reverse();
        }
        root.total += v;
        let mut at = &mut root;
        for n in names {
            at = at.children.entry(n).or_default();
            at.total += v;
        }
    }
    root
}

/// `tree`: the call tree, outermost first (`reverse`: from the leaves, callers below).
pub fn tree(view: &View, ty: usize, min_fraction: f64, reverse: bool) -> String {
    let u = unit(view, ty);
    let root = build(view, ty, reverse);
    let total = total(view, ty);
    let cut = (total as f64 * min_fraction) as i64;
    let mut out = String::new();
    fn walk(out: &mut String, node: &Node<'_>, depth: usize, cut: i64, total: i64, u: &str) {
        let mut kids: Vec<(&&str, &Node<'_>)> = node.children.iter().collect();
        kids.sort_by(|a, b| b.1.total.abs().cmp(&a.1.total.abs()).then(a.0.cmp(b.0)));
        let mut hidden = 0i64;
        for (name, kid) in kids {
            if kid.total.abs() < cut {
                hidden += kid.total;
                continue;
            }
            let _ = writeln!(
                out,
                "{:>12} {:>7}  {}{name}",
                format_value(kid.total, u),
                pct(kid.total, total),
                "  ".repeat(depth)
            );
            walk(out, kid, depth + 1, cut, total, u);
        }
        if hidden != 0 {
            let _ = writeln!(
                out,
                "{:>12} {:>7}  {}(below {})",
                format_value(hidden, u),
                pct(hidden, total),
                "  ".repeat(depth),
                format_value(cut, u)
            );
        }
    }
    walk(&mut out, &root, 0, cut, total, u);
    out
}

/// Folded stacks (`a;b;c value`), the input of flamegraph.pl and inferno, sorted.
pub fn folded(view: &View, ty: usize) -> String {
    let mut by: BTreeMap<String, i64> = BTreeMap::new();
    for s in &view.samples {
        *by.entry(view.names(s).join(";")).or_default() += value(s, ty);
    }
    let mut out = String::new();
    for (stack, v) in by.into_iter().filter(|it| it.1 != 0) {
        let _ = writeln!(out, "{stack} {v}");
    }
    out
}

/// A flame graph in text, `width` columns wide: one row a depth, roots at the top, each box
/// as wide as its share and labeled with as much of its name as fits.
pub fn flame(view: &View, ty: usize, width: usize, max_depth: usize) -> String {
    let root = build(view, ty, false);
    let width = width.max(20);
    let mut rows: Vec<Vec<char>> = Vec::new();
    fn place(
        rows: &mut Vec<Vec<char>>,
        node: &Node<'_>,
        depth: usize,
        start: f64,
        scale: f64,
        width: usize,
        max_depth: usize,
    ) {
        if depth >= max_depth {
            return;
        }
        let mut kids: Vec<(&&str, &Node<'_>)> = node.children.iter().collect();
        kids.sort_by(|a, b| a.0.cmp(b.0));
        let mut x = start;
        for (name, kid) in kids {
            let w = kid.total.max(0) as f64 * scale;
            let (a, b) = (x.round() as usize, (x + w).round() as usize);
            if b > a {
                while rows.len() <= depth {
                    rows.push(vec![' '; width]);
                }
                let row = &mut rows[depth];
                let cells = b.min(width) - a.min(width);
                let label: Vec<char> = if cells >= 3 {
                    short(name).chars().take(cells - 2).collect()
                } else {
                    Vec::new()
                };
                for (i, c) in row[a.min(width)..b.min(width)].iter_mut().enumerate() {
                    *c = if i == 0 {
                        '['
                    } else if i + 1 == cells {
                        ']'
                    } else {
                        label.get(i - 1).copied().unwrap_or('-')
                    };
                }
                place(rows, kid, depth + 1, x, scale, width, max_depth);
            }
            x += w;
        }
    }
    if root.total > 0 {
        place(&mut rows, &root, 0, 0.0, width as f64 / root.total as f64, width, max_depth);
    }
    let mut out = String::new();
    for r in rows {
        let line: String = r.into_iter().collect();
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

/// A function's name without its path, for narrow places.
fn short(name: &str) -> &str {
    let mut depth = 0usize;
    let mut cut = 0;
    let bytes = name.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => cut = i + 2,
            _ => {}
        }
    }
    &name[cut.min(name.len())..]
}

/// `list <regex>`: each matching function's source, each line with the flat and cumulative
/// value of the samples at it. `source_root` is joined to relative file names.
pub fn list(view: &View, ty: usize, pattern: &Regex, source_root: Option<&Path>) -> String {
    let u = unit(view, ty);
    // (function, file) -> line -> (flat, cum)
    let mut by: BTreeMap<(&str, &str), BTreeMap<u32, (i64, i64)>> = BTreeMap::new();
    for s in &view.samples {
        let v = value(s, ty);
        let mut seen: Vec<(&str, &str, u32)> = Vec::new();
        for (i, id) in s.stack.iter().enumerate() {
            let f = &view.frames[*id as usize];
            let (Some(file), Some(line)) = (f.file.as_deref(), f.line) else { continue };
            if !pattern.is_match(&f.function) {
                continue;
            }
            let key = (f.function.as_str(), file, line);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let e = by.entry((f.function.as_str(), file)).or_default().entry(line).or_default();
            if i == 0 {
                e.0 += v;
            }
            e.1 += v;
        }
    }
    let mut out = String::new();
    if by.is_empty() {
        let _ = writeln!(out, "no function matching {pattern} has samples with line numbers");
    }
    for ((function, file), lines) in by {
        let (flat, cum) = lines.values().fold((0, 0), |a, b| (a.0 + b.0, a.1.max(b.1)));
        let _ = writeln!(
            out,
            "ROUTINE {function}\n  {file}: flat {}, cum {}",
            format_value(flat, u),
            format_value(cum, u)
        );
        let path = match source_root {
            Some(root) if Path::new(file).is_relative() => root.join(file),
            _ => Path::new(file).to_path_buf(),
        };
        let first = *lines.keys().next().unwrap_or(&1);
        let last = *lines.keys().next_back().unwrap_or(&1);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let from = first.saturating_sub(3).max(1);
                for (n, src) in text
                    .lines()
                    .enumerate()
                    .map(|(i, l)| (i as u32 + 1, l))
                    .filter(|(n, _)| *n >= from && *n <= last + 2)
                {
                    let (f, c) = lines.get(&n).copied().unwrap_or((0, 0));
                    let show = |x: i64| if x == 0 { ".".to_owned() } else { format_value(x, u) };
                    let _ = writeln!(out, "{:>12} {:>12} {n:>6}: {src}", show(f), show(c));
                }
            }
            Err(why) => {
                let _ = writeln!(
                    out,
                    "  (source {} not read: {why}; pass --source <dir>)",
                    path.display()
                );
                for (n, (f, c)) in &lines {
                    let _ = writeln!(
                        out,
                        "{:>12} {:>12} {n:>6}",
                        format_value(*f, u),
                        format_value(*c, u)
                    );
                }
            }
        }
    }
    out
}

/// `diff --base`: `view` minus `base`, as one view whose base samples are negated (what
/// `go tool pprof -diff_base` does), frames matched by function name. Both must have the same
/// sample types.
pub fn diff(view: &View, base: &View) -> Result<View, String> {
    let names = |v: &View| {
        v.sample_types.iter().map(|t| format!("{}/{}", t.name, t.unit)).collect::<Vec<_>>()
    };
    if names(view) != names(base) {
        return Err(format!(
            "the profiles have different sample types: {:?} and base {:?}",
            names(view),
            names(base)
        ));
    }
    let mut out = view.clone();
    let mut by_name: BTreeMap<String, u32> = BTreeMap::new();
    for (i, f) in out.frames.iter().enumerate() {
        by_name.entry(f.function.clone()).or_insert(i as u32);
    }
    let mut map = Vec::with_capacity(base.frames.len());
    for f in &base.frames {
        let id = *by_name.entry(f.function.clone()).or_insert_with(|| {
            out.frames.push(f.clone());
            out.frames.len() as u32 - 1
        });
        map.push(id);
    }
    for s in &base.samples {
        out.samples.push(Sample {
            stack: s.stack.iter().map(|id| map[*id as usize]).collect(),
            values: s.values.iter().map(|v| -v).collect(),
            labels: s.labels.clone(),
        });
    }
    out.canonicalize();
    Ok(out)
}
