//! Charts (content/schema.flecs, "Charts"): `RowChart` and `LineChart` drawn at build time, and the `Picker` that
//! shows one of several panels.
//!
//! The SVGs have no viewBox: heights are in pixels and every x is a percentage of the width, so text keeps the page's
//! size at any width (a phone does not shrink it) and the marks stretch. A row chart is HTML rows (label, an SVG track
//! with the marks, the values written out, the ratio), laid out by styles/site.css on a wide card and on a phone. A
//! line chart's paths, which take no percentages, sit in a nested SVG whose viewBox is stretched along x only
//! (`preserveAspectRatio="none"`), with strokes that do not scale. Colours are the subjects' slots (`--series-N` in
//! styles/tokens.css, light and dark); text is in the ink tokens, never a series colour. Each chart carries its
//! numbers as a table under it, which is also what client/chart.ts reads for a line chart's tip.

use crate::inline::esc;
use crate::js;
use crate::world::{Entity, Id, Scalar, World};

/// A subject (`Subject` on the entity a `Plots` pair names): its label, colour slot and dashing.
#[derive(Clone, Debug, PartialEq)]
struct Subject {
    path: String,
    label: String,
    slot: i64,
    dashed: bool,
}

/// The colour slots styles/tokens.css defines.
const SLOTS: i64 = 4;

fn subject(world: &World, e: &Entity) -> Result<Subject, String> {
    let target = e
        .targets("Plots")
        .and_then(|t| t.first())
        .ok_or_else(|| format!("{}: a chart value needs (Plots, <subject>)", e.path))?;
    subject_at(world, target, &e.path)
}

fn subject_at(world: &World, path: &str, wher: &str) -> Result<Subject, String> {
    let s = world
        .lookup(path)
        .map(|id| world.get(id))
        .ok_or_else(|| format!("{wher}: {path} names nothing"))?;
    let v = s.value("Subject").ok_or_else(|| format!("{wher}: {path} is not a Subject"))?;
    let slot = match v.get("slot") {
        Some(Scalar::Int(i)) => *i,
        Some(Scalar::Float(f)) => *f as i64,
        _ => 0,
    };
    if !(1..=SLOTS).contains(&slot) {
        return Err(format!("{path}: Subject slot {slot} is not 1 to {SLOTS}"));
    }
    Ok(Subject {
        path: path.to_string(),
        label: js::string(v.get("label")),
        slot,
        dashed: matches!(v.get("dashed"), Some(Scalar::Bool(true))),
    })
}

/// `v` rounded to the nearest `1 / per`, halves away from zero as a reader rounds them (5.25 is 5.3), not by the
/// float's binary value as `{:.1}` would (5.2).
fn half_up(v: f64, per: f64) -> f64 {
    (v * per).round() / per
}

/// The units a chart may be in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
    Seconds,
    Millis,
    Megabytes,
}

impl Unit {
    fn of(e: &Entity, comp: &str) -> Result<Unit, String> {
        match js::string(e.value(comp).and_then(|v| v.get("unit"))).as_str() {
            "s" => Ok(Unit::Seconds),
            "ms" => Ok(Unit::Millis),
            "MB" => Ok(Unit::Megabytes),
            u => Err(format!("{}: unit {u:?} is not s, ms or MB", e.path)),
        }
    }

    /// A value as the reader reads it: `69 ms`, `1.18 s`, `531 MB`, `3.53 GB`.
    fn format(self, v: f64) -> String {
        match self {
            Unit::Seconds if v < 1.0 => format!("{} ms", (v * 1000.0).round()),
            Unit::Seconds => format!("{:.2} s", half_up(v, 100.0)),
            Unit::Millis if v >= 1000.0 => Unit::Seconds.format(v / 1000.0),
            Unit::Millis if v < 10.0 => format!("{:.1} ms", half_up(v, 10.0)),
            Unit::Millis => format!("{} ms", v.round()),
            Unit::Megabytes if v >= 1000.0 => format!("{:.2} GB", half_up(v / 1000.0, 100.0)),
            Unit::Megabytes => format!("{} MB", v.round()),
        }
    }

    /// An axis label: as short as the value allows.
    fn tick(self, v: f64) -> String {
        let trim = |x: f64| js::number(format!("{x:.3}").parse::<f64>().unwrap_or(x));
        match self {
            _ if v == 0.0 => "0".into(),
            Unit::Seconds if v < 1.0 => format!("{} ms", trim(v * 1000.0)),
            Unit::Seconds => format!("{} s", trim(v)),
            Unit::Millis if v >= 1000.0 => format!("{} s", trim(v / 1000.0)),
            Unit::Millis => format!("{} ms", trim(v)),
            Unit::Megabytes if v >= 1000.0 => format!("{} GB", trim(v / 1000.0)),
            Unit::Megabytes => format!("{} MB", trim(v)),
        }
    }
}

/// A chart's `scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Scale {
    Linear,
    Log,
}

impl Scale {
    fn of(e: &Entity, comp: &str) -> Result<Scale, String> {
        match js::string(e.value(comp).and_then(|v| v.get("scale"))).as_str() {
            "linear" => Ok(Scale::Linear),
            "log" => Ok(Scale::Log),
            s => Err(format!("{}: scale {s:?} is not linear or log", e.path)),
        }
    }

    fn class(self) -> &'static str {
        match self {
            Scale::Linear => "linear",
            Scale::Log => "log",
        }
    }
}

/// 1, 2, 2.5 or 5 times a power of ten, at least `rough`.
fn nice_step(rough: f64) -> f64 {
    let power = 10f64.powf(rough.log10().floor());
    let f = rough / power;
    let k = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 2.5 {
        2.5
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    k * power
}

/// The values `m × 10^k` for each `m` in `mantissas`, from the one at or below `lo` to the one at or above `hi`.
fn ladder(lo: f64, hi: f64, mantissas: &[f64]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut k = lo.log10().floor() as i32 - 1;
    while out.last().is_none_or(|&v| v < hi * (1.0 - 1e-9)) {
        for &m in mantissas {
            out.push(m * 10f64.powi(k));
        }
        k += 1;
    }
    let first = out.iter().rposition(|&v| v <= lo * (1.0 + 1e-9)).unwrap_or(0);
    let last = out.iter().position(|&v| v >= hi * (1.0 - 1e-9)).unwrap_or(out.len() - 1);
    out[first..=last].to_vec()
}

/// An axis: where a value sits along it (0 to 1), and its ticks.
#[derive(Clone, Debug, PartialEq)]
struct Axis {
    scale: Scale,
    lo: f64,
    hi: f64,
    ticks: Vec<f64>,
}

impl Axis {
    /// Linear: zero to a round number at or above the largest value, about three steps. Log: from the 1-2-5 value at
    /// or below the smallest to the one at or above the largest, ticked at as fine a 1-2-5, 1-3 or decade ladder as
    /// keeps to five ticks. A log axis takes values above zero only.
    fn new(scale: Scale, values: impl Iterator<Item = f64>, wher: &str) -> Result<Axis, String> {
        let (mut min, mut max) = (f64::INFINITY, 0.0f64);
        for v in values {
            min = min.min(v);
            max = max.max(v);
        }
        match scale {
            Scale::Linear => {
                if !(max > 0.0) {
                    return Ok(Axis { scale, lo: 0.0, hi: 1.0, ticks: vec![0.0, 1.0] });
                }
                let step = nice_step(max / 3.0);
                let hi = (max / step - 1e-9).ceil().max(1.0) * step;
                let ticks = (0..=((hi / step).round() as usize)).map(|i| i as f64 * step).collect();
                Ok(Axis { scale, lo: 0.0, hi, ticks })
            }
            Scale::Log => {
                if !(min > 0.0) {
                    return Err(format!("{wher}: a log scale takes values above zero only"));
                }
                let mut ends = ladder(min, max, &[1.0, 2.0, 5.0]);
                if ends.len() < 2 {
                    ends = ladder(min / 2.0, max * 2.0, &[1.0, 2.0, 5.0]);
                }
                let (lo, hi) = (ends[0], ends[ends.len() - 1]);
                let inside = |m: &[f64]| -> Vec<f64> {
                    ladder(lo, hi, m)
                        .into_iter()
                        .filter(|&v| v >= lo * (1.0 - 1e-9) && v <= hi * (1.0 + 1e-9))
                        .collect()
                };
                let ticks = [&[1.0, 2.0, 5.0][..], &[1.0, 3.0], &[1.0]]
                    .iter()
                    .map(|m| inside(m))
                    .find(|t| t.len() <= 5)
                    .unwrap_or_else(|| inside(&[1.0]));
                Ok(Axis { scale, lo, hi, ticks })
            }
        }
    }

    fn at(&self, v: f64) -> f64 {
        let f = match self.scale {
            Scale::Linear => (v - self.lo) / (self.hi - self.lo),
            Scale::Log => (v / self.lo).ln() / (self.hi / self.lo).ln(),
        };
        f.clamp(0.0, 1.0)
    }
}

fn pct(x: f64) -> String {
    format!("{}%", js::number((x * 10000.0).round() / 100.0))
}

fn f1(v: f64) -> String {
    js::number((v * 10.0).round() / 10.0)
}

/// `cargo / blaze` as a reader says it: `11× faster`, `2.1× slower`, `about the same`.
fn speedup(baseline: f64, lead: f64) -> Option<String> {
    if !(baseline > 0.0 && lead > 0.0) {
        return None;
    }
    let times = |r: f64| if r >= 10.0 { format!("{r:.0}×") } else { format!("{r:.1}×") };
    let r = baseline / lead;
    Some(if r >= 1.05 {
        format!("{} faster", times(r))
    } else if r <= 1.0 / 1.05 {
        format!("{} slower", times(1.0 / r))
    } else {
        "about the same".into()
    })
}

/// How a subject is drawn, for its key.
#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Bar,
    Dot,
    Line,
}

/// The key beside a subject's name: a bar swatch, a dot, or a line (dashed or not).
fn key(s: &Subject, mark: Mark) -> String {
    let dash = if s.dashed { " dashed" } else { "" };
    let (w, shape) = match mark {
        Mark::Bar => (
            12,
            format!(
                "<rect x=\"0\" y=\"1\" width=\"12\" height=\"8\" rx=\"2\" class=\"fill s{}\" />",
                s.slot
            ),
        ),
        Mark::Dot => {
            (10, format!("<circle cx=\"5\" cy=\"5\" r=\"4.5\" class=\"fill s{}\" />", s.slot))
        }
        Mark::Line => (
            18,
            format!(
                "<line x1=\"1\" y1=\"5\" x2=\"17\" y2=\"5\" class=\"stroke s{}{dash}\" />",
                s.slot
            ),
        ),
    };
    format!("<svg class=\"key\" width=\"{w}\" height=\"10\" aria-hidden=\"true\">{shape}</svg>")
}

/// The caption: title, note, and a legend when the chart compares two subjects or more.
fn caption(title: &str, note: &str, subjects: &[Subject], mark: Mark) -> String {
    let legend = if subjects.len() > 1 {
        let items: String = subjects
            .iter()
            .map(|s| format!("<li>{}{}</li>", key(s, mark), esc(&s.label)))
            .collect();
        format!("<ul class=\"legend\">{items}</ul>")
    } else {
        String::new()
    };
    let note =
        if note.is_empty() { String::new() } else { format!("<p class=\"note\">{note}</p>") };
    format!("<figcaption><span class=\"title\">{}</span>{note}{legend}</figcaption>", esc(title))
}

/// The numbers, in a fold under the chart.
fn table(head: &[String], rows: &[Vec<String>]) -> String {
    let th: String = head.iter().map(|h| format!("<th scope=\"col\">{h}</th>")).collect();
    let body: String = rows
        .iter()
        .map(|r| {
            let (first, rest) = r.split_first().expect("a row has its label");
            let tds: String = rest.iter().map(|c| format!("<td class=\"num\">{c}</td>")).collect();
            format!("<tr><th scope=\"row\">{first}</th>{tds}</tr>")
        })
        .collect();
    format!(
        "<details class=\"numbers\"><summary>Numbers</summary><div class=\"ref\"><table><thead><tr>{th}</tr></thead><tbody>{body}</tbody></table></div></details>"
    )
}

fn notes_html(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let items: Vec<String> =
        notes.iter().enumerate().map(|(i, n)| format!("{} {}", mark(i), esc(n))).collect();
    format!("<p class=\"note\">{}</p>", items.join("; "))
}

fn mark(i: usize) -> &'static str {
    ["*", "†", "‡", "§"].get(i).copied().unwrap_or("¶")
}

/// The notes of a chart's values, numbered in order of first use.
#[derive(Default)]
struct Notes(Vec<String>);

impl Notes {
    fn mark(&mut self, note: &str) -> &'static str {
        if note.is_empty() {
            return "";
        }
        let i = match self.0.iter().position(|n| n == note) {
            Some(i) => i,
            None => {
                self.0.push(note.to_string());
                self.0.len() - 1
            }
        };
        mark(i)
    }
}

/// Row geometry, in pixels: a line per subject (its bar, and its value beside the track), and the track's padding
/// above and below, inside the SVG so that the grid runs on from row to row.
const LINE: f64 = 17.0;
const PAD: f64 = 5.0;
const BAR_H: f64 = 10.0;
const DOT_R: f64 = 5.0;
const AXIS_H: f64 = 16.0;

/// The anchor of a tick label at `f` along a track: the ends keep inside it.
fn anchor(f: f64) -> &'static str {
    if f < 0.04 {
        "start"
    } else if f > 0.96 {
        "end"
    } else {
        "middle"
    }
}

/// A `RowChart`: per `Row` its label, an SVG track with its values (bars from zero on a linear scale, dots joined
/// from the smallest to the largest on a log one), the values written out, and its speedup over the chart's
/// `Baseline`. The rows share one axis, under them.
pub fn row_chart(world: &World, n: Id, id: &str, note: &str) -> Result<String, String> {
    let e = world.get(n);
    let v = e.value("RowChart").expect("a RowChart");
    let title = js::string(v.get("title"));
    let unit = Unit::of(e, "RowChart")?;
    let scale = Scale::of(e, "RowChart")?;
    let baseline = match e.targets("Baseline").and_then(|t| t.first()) {
        Some(p) => Some(subject_at(world, p, &e.path)?),
        None => None,
    };

    struct Row {
        label: String,
        values: Vec<(Subject, f64, String)>,
    }
    let mut rows = Vec::new();
    let mut subjects: Vec<Subject> = Vec::new();
    for &r in &e.children {
        let re = world.get(r);
        let label = js::string(
            re.value("Row")
                .ok_or_else(|| format!("{}: a RowChart's children are Rows", re.path))?
                .get("label"),
        );
        let mut values = Vec::new();
        for &b in &re.children {
            let be = world.get(b);
            let bv = be
                .value("Value")
                .ok_or_else(|| format!("{}: a Row's children are Values", be.path))?;
            let value = js::to_number(bv.get("value"));
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{}: a Value must be zero or more", be.path));
            }
            let s = subject(world, be)?;
            if values.iter().any(|(x, _, _): &(Subject, f64, String)| x.path == s.path) {
                return Err(format!("{}: two Values for {} in one Row", be.path, s.path));
            }
            if !subjects.iter().any(|x| x.path == s.path) {
                subjects.push(s.clone());
            }
            values.push((s, value, js::string(bv.get("note"))));
        }
        rows.push(Row { label, values });
    }
    if rows.is_empty() {
        return Err(format!("{}: a RowChart with no Rows", e.path));
    }
    // Every row lists its values in the legend's order, so a colour sits in the same place in every row.
    for r in &mut rows {
        r.values.sort_by_key(|(s, _, _)| subjects.iter().position(|x| x.path == s.path));
    }
    let axis = Axis::new(scale, rows.iter().flat_map(|r| r.values.iter().map(|b| b.1)), &e.path)?;
    let ratio = |r: &Row| {
        let b = baseline.as_ref()?;
        let base = r.values.iter().find(|(s, _, _)| s.path == b.path)?.1;
        let lead = r.values.iter().find(|(s, _, _)| s.path != b.path)?.1;
        speedup(base, lead)
    };
    let mark_of = if scale == Scale::Log { Mark::Dot } else { Mark::Bar };
    let track_h = subjects.len() as f64 * LINE + 2.0 * PAD;
    let grid: String = axis
        .ticks
        .iter()
        .map(|&t| {
            let x = pct(axis.at(t));
            let zero = if scale == Scale::Linear && t == 0.0 { " zero" } else { "" };
            format!("<line class=\"grid{zero}\" x1=\"{x}\" x2=\"{x}\" y1=\"0\" y2=\"100%\" />")
        })
        .collect();

    let mut notes = Notes::default();
    let mut body = String::new();
    let mut table_rows = Vec::new();
    for r in &rows {
        let speed = ratio(r);
        let mut marks = String::new();
        let mut vals = String::new();
        let mut cells = vec![esc(&r.label)];
        for (k, s) in subjects.iter().enumerate() {
            let found = r.values.iter().find(|(b, _, _)| b.path == s.path);
            let text = match found {
                Some((_, v, note)) => format!("{}{}", esc(&unit.format(*v)), notes.mark(note)),
                None => "–".into(),
            };
            vals += &format!("<span>{}{text}</span>", key(s, mark_of));
            cells.push(text);
            if let (Some((_, v, _)), Scale::Linear) = (found, scale) {
                marks += &format!(
                    "<rect class=\"fill s{}\" x=\"0\" y=\"{}\" width=\"{}\" height=\"{BAR_H}\" rx=\"2\" />",
                    s.slot,
                    f1(PAD + k as f64 * LINE + (LINE - BAR_H) / 2.0),
                    pct(axis.at(*v))
                );
            }
        }
        if scale == Scale::Log {
            let cy = f1(track_h / 2.0);
            let xs: Vec<f64> = r.values.iter().map(|(_, v, _)| axis.at(*v)).collect();
            let (a, b) = xs.iter().fold((1.0f64, 0.0f64), |(a, b), &x| (a.min(x), b.max(x)));
            if xs.len() > 1 {
                marks += &format!(
                    "<line class=\"rc-span\" x1=\"{}\" x2=\"{}\" y1=\"{cy}\" y2=\"{cy}\" />",
                    pct(a),
                    pct(b)
                );
            }
            // The first subject last, so that it is on top where dots meet.
            for (s, v, _) in r.values.iter().rev() {
                marks += &format!(
                    "<circle class=\"dot s{}\" cx=\"{}\" cy=\"{cy}\" r=\"{DOT_R}\" />",
                    s.slot,
                    pct(axis.at(*v))
                );
            }
        }
        if baseline.is_some() {
            cells.push(speed.as_deref().map(esc).unwrap_or_else(|| "–".into()));
        }
        table_rows.push(cells);
        let ratio_html = match (&baseline, &speed) {
            (None, _) => String::new(),
            (Some(_), sp) => format!(
                "<span class=\"rc-ratio\">{}</span>",
                sp.as_deref().map(esc).unwrap_or_default()
            ),
        };
        body += &format!(
            "<div class=\"rc-row\"><span class=\"rc-label\">{}</span><svg class=\"rc-track\" width=\"100%\" height=\"{}\" aria-hidden=\"true\">{grid}{marks}</svg><span class=\"rc-vals\">{vals}</span>{ratio_html}</div>",
            esc(&r.label),
            f1(track_h)
        );
    }
    let ticks: String = axis
        .ticks
        .iter()
        .map(|&t| {
            let f = axis.at(t);
            format!(
                "<text class=\"tick\" x=\"{}\" y=\"11\" text-anchor=\"{}\">{}</text>",
                pct(f),
                anchor(f),
                esc(&unit.tick(t))
            )
        })
        .collect();
    let axis_row = format!(
        "<div class=\"rc-row rc-axis\" aria-hidden=\"true\"><svg class=\"rc-track\" width=\"100%\" height=\"{AXIS_H}\">{ticks}</svg></div>"
    );
    let names: Vec<&str> = subjects.iter().map(|s| s.label.as_str()).collect();
    let label = format!(
        "{title}: {} for each of {} rows, on a {} scale; the numbers are in the table below.",
        names.join(" and "),
        rows.len(),
        scale.class()
    );
    let mut head = vec![String::new()];
    head.extend(subjects.iter().map(|s| esc(&s.label)));
    if let Some(b) = &baseline {
        head.push(format!("vs {}", esc(&b.label)));
    }
    Ok(format!(
        "<figure class=\"chart rows {}{}\" id=\"{}\">{}<div class=\"plot\" role=\"img\" aria-label=\"{}\">{body}{axis_row}</div>{}{}</figure>",
        scale.class(),
        if baseline.is_some() { " ratios" } else { "" },
        esc(id),
        caption(&title, note, &subjects, mark_of),
        esc(&label),
        notes_html(&notes.0),
        table(&head, &table_rows)
    ))
}

/// Line geometry, in pixels.
const LINE_H: f64 = 168.0;
const TOP: f64 = 14.0;
const BOTTOM: f64 = 22.0;
/// The share of the width at each side kept clear, so the first and last points are whole.
const EDGE: f64 = 0.02;
/// Every point gets a dot up to this many runs; past it only the last does.
const DOTS_UP_TO: usize = 24;

/// One number per run, `-` (or anything not a number) where there is none.
fn numbers(s: &str, n: usize, wher: &str) -> Result<Vec<Option<f64>>, String> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: Vec<Option<f64>> = s
        .split_whitespace()
        .map(|t| t.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0))
        .collect();
    if v.len() != n {
        return Err(format!("{wher}: {} values for {n} runs", v.len()));
    }
    Ok(v)
}

/// Runs of consecutive indices where `ok(i)`: a missing run breaks the line.
fn stretches(n: usize, ok: impl Fn(usize) -> bool) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for i in 0..n {
        if ok(i) {
            cur.push(i);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `Oct 20` from an ISO date, as it was written (UTC).
fn day(iso: &str) -> String {
    let (m, d) = (iso.get(5..7).and_then(|m| m.parse::<usize>().ok()), iso.get(8..10));
    match (m, d.and_then(|d| d.parse::<u32>().ok())) {
        (Some(m @ 1..=12), Some(d)) => format!("{} {d}", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

/// A run as the table and a one-run chart name it: its day and a link to its commit.
fn run_html(date: &str, commit: &str) -> String {
    if commit == "-" {
        return esc(&day(date));
    }
    let short: String = commit.chars().take(10).collect();
    format!(
        "{} <a href=\"https://github.com/indexable-inc/blaze/commit/{}\"><code>{}</code></a>",
        esc(&day(date)),
        esc(commit),
        esc(&short)
    )
}

/// A `LineChart`: a line per `Series` over the chart's `Runs`, with its band when it has one. With one run there is
/// no line to draw: each subject's value is written out instead.
pub fn line_chart(world: &World, n: Id, id: &str, note: &str) -> Result<String, String> {
    let e = world.get(n);
    let v = e.value("LineChart").expect("a LineChart");
    let title = js::string(v.get("title"));
    let unit = Unit::of(e, "LineChart")?;
    let scale = Scale::of(e, "LineChart")?;
    let runs = e.value("Runs").ok_or_else(|| format!("{}: a LineChart needs Runs", e.path))?;
    let dates: Vec<String> =
        js::string(runs.get("dates")).split_whitespace().map(str::to_string).collect();
    let commits: Vec<String> =
        js::string(runs.get("commits")).split_whitespace().map(str::to_string).collect();
    let n_runs = dates.len();
    if n_runs == 0 || commits.len() != n_runs {
        return Err(format!("{}: Runs has {n_runs} dates and {} commits", e.path, commits.len()));
    }

    struct Line {
        s: Subject,
        values: Vec<Option<f64>>,
        band: Option<(Vec<Option<f64>>, Vec<Option<f64>>)>,
    }
    let mut lines: Vec<Line> = Vec::new();
    for &c in &e.children {
        let ce = world.get(c);
        let sv = ce
            .value("Series")
            .ok_or_else(|| format!("{}: a LineChart's children are Series", ce.path))?;
        let s = subject(world, ce)?;
        if lines.iter().any(|l| l.s.path == s.path) {
            return Err(format!("{}: two Series for {}", ce.path, s.path));
        }
        let values = numbers(&js::string(sv.get("values")), n_runs, &ce.path)?;
        let (lo, hi) = (
            numbers(&js::string(sv.get("lo")), n_runs, &ce.path)?,
            numbers(&js::string(sv.get("hi")), n_runs, &ce.path)?,
        );
        if values.is_empty() {
            return Err(format!("{}: a Series with no values", ce.path));
        }
        let band = if lo.is_empty() || hi.is_empty() { None } else { Some((lo, hi)) };
        lines.push(Line { s, values, band });
    }
    if lines.is_empty() {
        return Err(format!("{}: a LineChart with no Series", e.path));
    }
    let subjects: Vec<Subject> = lines.iter().map(|l| l.s.clone()).collect();

    if n_runs == 1 {
        let stats: String = lines
            .iter()
            .map(|l| {
                let value = l.values[0].map(|v| esc(&unit.format(v))).unwrap_or_else(|| "–".into());
                format!(
                    "<div><dt>{}{}</dt><dd>{value}</dd></div>",
                    key(&l.s, Mark::Line),
                    esc(&l.s.label)
                )
            })
            .collect();
        return Ok(format!(
            "<figure class=\"chart lines one\" id=\"{}\">{}<dl class=\"stats\">{stats}</dl><p class=\"note\">One run so far: {}. A line starts with the next.</p></figure>",
            esc(id),
            caption(&title, note, &[], Mark::Line),
            run_html(&dates[0], &commits[0])
        ));
    }

    let axis = Axis::new(
        scale,
        lines.iter().flat_map(|l| {
            l.values
                .iter()
                .chain(l.band.iter().flat_map(|(lo, hi)| lo.iter().chain(hi.iter())))
                .flatten()
                .copied()
        }),
        &e.path,
    )?;
    let ph = LINE_H - TOP - BOTTOM;
    // x as a share of the width; y in pixels inside the plot.
    let x = |i: usize| EDGE + i as f64 / (n_runs - 1) as f64 * (1.0 - 2.0 * EDGE);
    let y = |v: f64| ph - axis.at(v) * ph;
    // In the stretched inner SVG, x runs 0 to 1000.
    let ix = |i: usize| f1(x(i) * 1000.0);

    let mut grid = String::new();
    for &t in &axis.ticks {
        let ty = TOP + y(t);
        let zero = scale == Scale::Linear && t == 0.0;
        grid += &format!(
            "<line class=\"grid{}\" x1=\"0\" x2=\"100%\" y1=\"{}\" y2=\"{}\" />",
            if zero { " zero" } else { "" },
            f1(ty),
            f1(ty)
        );
        if !zero {
            grid += &format!(
                "<text class=\"tick\" x=\"0\" y=\"{}\">{}</text>",
                f1(ty - 4.0),
                esc(&unit.tick(t))
            );
        }
    }
    let base = f1(LINE_H - 6.0);
    grid += &format!(
        "<text class=\"tick\" x=\"{}\" y=\"{base}\">{}</text><text class=\"tick\" x=\"{}\" y=\"{base}\" text-anchor=\"end\">{}</text>",
        pct(x(0)),
        esc(&day(&dates[0])),
        pct(x(n_runs - 1)),
        esc(&day(&dates[n_runs - 1]))
    );

    // Bands first, then the lines, the first subject last so that it is on top.
    let mut inner = String::new();
    for l in lines.iter().rev() {
        let Some((lo, hi)) = &l.band else { continue };
        for st in stretches(n_runs, |i| lo[i].is_some() && hi[i].is_some()) {
            let (a, b) = (st[0], st[st.len() - 1]);
            // One run alone is a whisker, drawn with the dots below.
            if a == b {
                continue;
            }
            let upper: Vec<String> =
                st.iter().map(|&i| format!("{},{}", ix(i), f1(y(hi[i].unwrap_or(0.0))))).collect();
            let lower: Vec<String> = st
                .iter()
                .rev()
                .map(|&i| format!("{},{}", ix(i), f1(y(lo[i].unwrap_or(0.0)))))
                .collect();
            inner += &format!(
                "<path class=\"band s{}\" d=\"M{}L{}Z\" />",
                l.s.slot,
                upper.join("L"),
                lower.join("L")
            );
        }
    }
    for l in lines.iter().rev() {
        for st in stretches(n_runs, |i| l.values[i].is_some()) {
            if st.len() < 2 {
                continue;
            }
            let d: Vec<String> = st
                .iter()
                .map(|&i| format!("{},{}", ix(i), f1(y(l.values[i].unwrap_or(0.0)))))
                .collect();
            inner += &format!(
                "<path class=\"stroke s{}{}\" d=\"M{}\" vector-effect=\"non-scaling-stroke\" />",
                l.s.slot,
                if l.s.dashed { " dashed" } else { "" },
                d.join("L")
            );
        }
    }
    let mut dots = String::new();
    for l in lines.iter().rev() {
        if let Some((lo, hi)) = &l.band {
            for i in 0..n_runs {
                let alone = (i == 0 || lo[i - 1].is_none() || hi[i - 1].is_none())
                    && (i + 1 == n_runs || lo[i + 1].is_none() || hi[i + 1].is_none());
                if let (true, Some(a), Some(b)) = (alone, lo[i], hi[i]) {
                    dots += &format!(
                        "<line class=\"whisker s{}\" x1=\"{p}\" x2=\"{p}\" y1=\"{}\" y2=\"{}\" />",
                        l.s.slot,
                        f1(TOP + y(b)),
                        f1(TOP + y(a)),
                        p = pct(x(i))
                    );
                }
            }
        }
        for (i, v) in l.values.iter().enumerate() {
            let Some(v) = v else { continue };
            if n_runs > DOTS_UP_TO && i + 1 != n_runs {
                continue;
            }
            dots += &format!(
                "<circle class=\"dot s{}\" cx=\"{}\" cy=\"{}\" r=\"4\" />",
                l.s.slot,
                pct(x(i)),
                f1(TOP + y(*v))
            );
        }
    }

    let mut head = vec!["Run".to_string()];
    head.extend(subjects.iter().map(|s| esc(&s.label)));
    let rows: Vec<Vec<String>> = (0..n_runs)
        .rev()
        .map(|i| {
            let mut r = vec![run_html(&dates[i], &commits[i])];
            for l in &lines {
                let band = l.band.as_ref().and_then(|(lo, hi)| Some((lo[i]?, hi[i]?)));
                r.push(match (l.values[i], band) {
                    (Some(v), Some((a, b))) => format!(
                        "{} <span class=\"dim\">({} to {})</span>",
                        esc(&unit.format(v)),
                        esc(&unit.format(a)),
                        esc(&unit.format(b))
                    ),
                    (Some(v), None) => esc(&unit.format(v)),
                    (None, _) => "–".into(),
                });
            }
            r
        })
        .collect();
    let names: Vec<&str> = subjects.iter().map(|s| s.label.as_str()).collect();
    let label = format!(
        "{title}: {} over {n_runs} runs, from {} to {}, on a {} scale; the numbers are in the table below.",
        names.join(" and "),
        day(&dates[0]),
        day(&dates[n_runs - 1]),
        scale.class()
    );
    Ok(format!(
        "<figure class=\"chart lines\" id=\"{}\">{}<svg class=\"plot\" width=\"100%\" height=\"{LINE_H}\" role=\"img\" aria-label=\"{}\" data-runs=\"{n_runs}\" data-edge=\"{EDGE}\">{grid}<svg y=\"{TOP}\" width=\"100%\" height=\"{}\" viewBox=\"0 0 1000 {}\" preserveAspectRatio=\"none\" overflow=\"visible\">{inner}</svg><line class=\"guide\" x1=\"0\" x2=\"0\" y1=\"{TOP}\" y2=\"{}\" />{dots}</svg>{}</figure>",
        esc(id),
        caption(&title, note, &subjects, Mark::Line),
        esc(&label),
        f1(ph),
        f1(ph),
        f1(TOP + ph),
        table(&head, &rows)
    ))
}

/// The most Picks a Picker may have: styles/site.css shows the panel at the same place as the checked radio button,
/// with a rule per place.
pub const MAX_PICKS: usize = 16;

/// A `Picker`'s buttons, after its label: radio inputs named for the picker, so it needs no JS; the panels follow
/// (render.rs).
pub fn picker_buttons(world: &World, n: Id, id: &str, picks: &[Id]) -> Result<String, String> {
    let e = world.get(n);
    let label = js::string(e.value("Picker").and_then(|v| v.get("label")));
    if picks.is_empty() || picks.len() > MAX_PICKS {
        return Err(format!(
            "{}: a Picker has 1 to {MAX_PICKS} Picks, not {}",
            e.path,
            picks.len()
        ));
    }
    let chosen = picks.iter().position(|&p| world.get(p).has_tag("Picked")).unwrap_or(0);
    let mut buttons = String::new();
    for (i, &p) in picks.iter().enumerate() {
        let pl = js::string(world.get(p).value("Pick").and_then(|v| v.get("label")));
        buttons += &format!(
            "<label><input type=\"radio\" name=\"pick-{}\" value=\"{i}\"{} /><span>{}</span></label>",
            esc(id),
            if i == chosen { " checked" } else { "" },
            esc(&pl)
        );
    }
    Ok(format!(
        "<fieldset class=\"picks\"><legend class=\"sr\">{l}</legend><span class=\"what\" aria-hidden=\"true\">{l}</span>{buttons}</fieldset>",
        l = esc(&label)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_readme() {
        assert_eq!(Unit::Seconds.format(0.069), "69 ms");
        assert_eq!(Unit::Seconds.format(1.181), "1.18 s");
        assert_eq!(Unit::Millis.format(5.25), "5.3 ms");
        assert_eq!(Unit::Millis.format(1526.27), "1.53 s");
        assert_eq!(Unit::Megabytes.format(3532.0), "3.53 GB");
        assert_eq!(Unit::Megabytes.format(531.4), "531 MB");
        assert_eq!(Unit::Seconds.tick(0.25), "250 ms");
        assert_eq!(Unit::Megabytes.tick(2500.0), "2.5 GB");
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-9 * y.abs().max(1.0))
    }

    #[test]
    fn linear_axes_start_at_zero_and_end_round() {
        let a = Axis::new(Scale::Linear, [0.787].into_iter(), "t").unwrap();
        assert!(close(&a.ticks, &[0.0, 0.5, 1.0]) && a.hi == 1.0);
        assert_eq!(
            Axis::new(Scale::Linear, [133.9].into_iter(), "t").unwrap().ticks,
            vec![0.0, 50.0, 100.0, 150.0]
        );
        assert_eq!(Axis::new(Scale::Linear, [0.0].into_iter(), "t").unwrap().ticks, vec![0.0, 1.0]);
        assert_eq!(pct(0.787), "78.7%");
    }

    #[test]
    fn log_axes_hold_every_value_on_round_ends() {
        // 25 ms beside 1.31 s: 20 ms to 2 s, ticked 30 ms, 100 ms, 300 ms, 1 s.
        let a = Axis::new(Scale::Log, [0.025, 1.31].into_iter(), "t").unwrap();
        assert!(close(&[a.lo, a.hi], &[0.02, 2.0]));
        assert!(close(&a.ticks, &[0.03, 0.1, 0.3, 1.0]));
        assert!((a.at(0.2) - 0.5).abs() < 1e-9);
        // Close together: the 1-2-5 ladder.
        let b = Axis::new(Scale::Log, [0.025, 0.036].into_iter(), "t").unwrap();
        assert!(close(&[b.lo, b.hi], &[0.02, 0.05]) && close(&b.ticks, &[0.02, 0.05]));
        // One value on a rung: widened to the rungs around it.
        let c = Axis::new(Scale::Log, [0.1].into_iter(), "t").unwrap();
        assert!(close(&[c.lo, c.hi], &[0.05, 0.2]));
        assert!(Axis::new(Scale::Log, [0.0, 1.0].into_iter(), "t").is_err());
    }

    #[test]
    fn speedups() {
        assert_eq!(speedup(0.787, 0.069).as_deref(), Some("11× faster"));
        assert_eq!(speedup(23.7, 105.7).as_deref(), Some("4.5× slower"));
        assert_eq!(speedup(1.0, 1.02).as_deref(), Some("about the same"));
        assert_eq!(speedup(0.0, 1.0), None);
    }
}
