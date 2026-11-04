//! `blaze debug time --html`: one self-contained page (inline CSS and SVG, nothing fetched) of
//! a build's time: the cores' busy strip, the critical path, and per unit a bar of running,
//! uncovered, parked, queued and unrecorded time; then the build's kept `tracing` events.

use std::fmt::Write as _;

use super::{
    Model, Totals,
    path::{LinkKind, Path},
    report::{self, By, Row},
};

/// The colors of the states, as the legend and the bars draw them.
const COVERED: &str = "#2f7d32";
const UNCOVERED: &str = "#d98c1f";
const PARKED: &str = "#5470c6";
const QUEUED: &str = "#c23b3b";
const PARKED_OR_QUEUED: &str = "#8e7cc3";
const UNRECORDED: &str = "#9e9e9e";

const WIDTH: f64 = 1000.0;

/// `text` with `&`, `<`, `>` and `"` escaped.
fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn kind_color(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Ran => COVERED,
        LinkKind::Queued | LinkKind::Spawned => QUEUED,
        LinkKind::Waited => PARKED_OR_QUEUED,
        LinkKind::Wake => PARKED,
        LinkKind::Unrecorded => UNRECORDED,
    }
}

/// The page of `model`, its `path`, and its `top` units.
pub fn page(model: &Model, path: &Path, top: usize) -> String {
    // alloc: the page, once a report
    let mut out = String::with_capacity(1 << 16);
    out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>");
    // Writing to a `String` does not fail.
    _ = write!(out, "blaze build {} time", model.build);
    out.push_str(
        "</title><style>\
         body{font:14px/1.4 system-ui,sans-serif;margin:24px;color:#222;background:#fff}\
         @media (prefers-color-scheme:dark){body{color:#ddd;background:#161616}}\
         pre{font:12px/1.35 ui-monospace,monospace;overflow-x:auto}\
         table{border-collapse:collapse;font:12px ui-monospace,monospace}\
         td,th{padding:2px 8px;text-align:right}td:last-child,th:last-child{text-align:left}\
         .legend span{display:inline-block;margin-right:14px}\
         .legend i{display:inline-block;width:10px;height:10px;margin-right:4px}\
         svg{display:block;margin:6px 0 18px}\
         </style></head><body>\n",
    );
    _ = writeln!(out, "<h1>Build {}</h1>", model.build);
    let mut summary = String::new();
    report::summary(model, &mut summary);
    out.push_str("<pre>");
    escape(&summary, &mut out);
    out.push_str("</pre>\n");
    legend(&mut out);
    cores(model, &mut out);
    critical(model, path, top, &mut out);
    units(model, top, &mut out);
    events(model, &mut out);
    out.push_str("</body></html>\n");
    out
}

/// The build's kept `tracing` events in time order: the first [`EVENTS_SHOWN`] of them.
fn events(model: &Model, out: &mut String) {
    const EVENTS_SHOWN: usize = 500;
    let shown = model.events.len().min(EVENTS_SHOWN);
    _ = writeln!(
        out,
        "<h2>Events</h2><p>The first {shown} of the build's {} kept events (error, warn, info, and debug when written), each with its fields.</p>",
        model.events.len()
    );
    if shown == 0 {
        return;
    }
    out.push_str(
        "<table><tr><th>at</th><th>level</th><th>on</th><th>target</th><th>message</th></tr>\n",
    );
    for (it, lane) in model.events.iter().zip(&model.event_lane).take(shown) {
        let on = match lane {
            // alloc: a cell, once an event a report
            Some(lane) => model.lanes[*lane].id.to_string(),
            // alloc: a cell, once an event a report
            None => format!("thread {}", it.thread),
        };
        _ = write!(out, "<tr><td>{:.3} s</td><td>", (it.t_ns - model.bounds.start_ns) as f64 / 1e9);
        for text in [it.level.as_str(), on.as_str(), it.target.as_str()] {
            escape(text, out);
            out.push_str("</td><td>");
        }
        escape(&it.said(), out);
        out.push_str("</td></tr>\n");
    }
    out.push_str("</table>\n");
}

fn legend(out: &mut String) {
    out.push_str("<p class=\"legend\">");
    for (color, name) in [
        (COVERED, "running, under a child span"),
        (UNCOVERED, "running, uncovered"),
        (PARKED, "parked (or a wake's latency on the path)"),
        (QUEUED, "queued"),
        (PARKED_OR_QUEUED, "parked or queued (no ready row)"),
        (UNRECORDED, "unrecorded"),
    ] {
        _ = write!(out, "<span><i style=\"background:{color}\"></i>{name}</span>");
    }
    out.push_str("</p>\n");
}

/// A row a core, each pixel-wide stretch as dark as the core was busy in it, under a row of
/// all cores together.
fn cores(model: &Model, out: &mut String) {
    const BUCKETS: i64 = 500;
    const ROW: f64 = 6.0;
    let width_ns = (model.bounds.wall_ns() / BUCKETS).max(1);
    let busy = report::core_busy(model, width_ns);
    let util = report::utilization(model);
    // alloc: once a report
    let mut keys: Vec<(u32, u32)> = busy.iter().map(|it| (it.process, it.core)).collect();
    keys.dedup();
    let height = ROW * (keys.len() as f64 + 2.0);
    _ = writeln!(
        out,
        "<h2>Cores</h2><p>{} cores ran jobs, {:.1}% busy on average. The top row is all cores together.</p>",
        util.cores,
        100.0 * util.ratio()
    );
    _ = writeln!(
        out,
        "<svg width=\"{WIDTH}\" height=\"{height}\" viewBox=\"0 0 {WIDTH} {height}\">"
    );
    let x =
        |t: i64| (t - model.bounds.start_ns) as f64 / model.bounds.wall_ns().max(1) as f64 * WIDTH;
    // alloc: once a report
    let mut all = std::collections::BTreeMap::<i64, (i64, i64)>::new();
    for it in &busy {
        let row =
            keys.iter().position(|key| *key == (it.process, it.core)).unwrap_or(0) as f64 + 2.0;
        let span = (it.t1_ns - it.t0_ns).max(1);
        let total = all.entry(it.t0_ns).or_insert((it.t1_ns, 0));
        total.1 += it.running_ns;
        if it.running_ns > 0 {
            _ = writeln!(
                out,
                "<rect x=\"{:.2}\" y=\"{:.1}\" width=\"{:.2}\" height=\"{:.1}\" fill=\"{COVERED}\" fill-opacity=\"{:.3}\"/>",
                x(it.t0_ns),
                row * ROW,
                x(it.t1_ns) - x(it.t0_ns),
                ROW - 1.0,
                it.running_ns as f64 / span as f64
            );
        }
    }
    for (t0, (t1, running)) in all {
        let share = running as f64 / ((t1 - t0).max(1) as f64 * f64::from(util.cores.max(1)));
        _ = writeln!(
            out,
            "<rect x=\"{:.2}\" y=\"0\" width=\"{:.2}\" height=\"{:.1}\" fill=\"{COVERED}\" fill-opacity=\"{share:.3}\"/>",
            x(t0),
            x(t1) - x(t0),
            ROW * 1.5
        );
    }
    out.push_str("</svg>\n");
}

/// The path as a strip, each pixel the color of the kind of link that held most of it, and its
/// longest links.
fn critical(model: &Model, path: &Path, top: usize, out: &mut String) {
    const PIXELS: usize = 1000;
    out.push_str("<h2>Critical path</h2><pre>");
    let mut text = String::new();
    report::path(model, path, top, &mut text);
    escape(&text, out);
    out.push_str("</pre>\n");
    let (Some(start), Some(end)) = (path.start_ns(), path.end_ns()) else { return };
    let per = ((end - start) as f64 / PIXELS as f64).max(1.0);
    // alloc: once a report
    let mut pixels = vec![[0i64; 5]; PIXELS];
    let kinds = LinkKind::ALL;
    for link in &path.links {
        let k = kinds.iter().position(|it| *it == link.kind).unwrap_or(0);
        let mut from = link.from_ns;
        while from < link.to_ns {
            let p = (((from - start) as f64 / per) as usize).min(PIXELS - 1);
            let pixel_end = start + ((p + 1) as f64 * per).ceil() as i64;
            let upto = link.to_ns.min(pixel_end.max(from + 1));
            pixels[p][k] += upto - from;
            from = upto;
        }
    }
    _ = writeln!(out, "<svg width=\"{WIDTH}\" height=\"18\" viewBox=\"0 0 {WIDTH} 18\">");
    for (p, it) in pixels.iter().enumerate() {
        let Some((k, _)) = it.iter().enumerate().filter(|it| *it.1 > 0).max_by_key(|it| *it.1)
        else {
            continue;
        };
        _ = writeln!(
            out,
            "<rect x=\"{:.1}\" y=\"0\" width=\"{:.2}\" height=\"18\" fill=\"{}\"/>",
            p as f64 * WIDTH / PIXELS as f64,
            WIDTH / PIXELS as f64,
            kind_color(kinds[k])
        );
    }
    out.push_str("</svg>\n");
}

/// The `top` units by wall time, each a bar of its time by state on one scale.
fn units(model: &Model, top: usize, out: &mut String) {
    const BAR: f64 = 14.0;
    const LABEL: f64 = 420.0;
    let rows = report::rows(model, By::Unit, None);
    let shown = &rows[..rows.len().min(top)];
    let most = shown.iter().map(|it| it.wall_ns).max().unwrap_or(1).max(1) as f64;
    let height = BAR * shown.len() as f64 + 4.0;
    let width = WIDTH + LABEL;
    _ = writeln!(
        out,
        "<h2>Units</h2><p>The {} longest unit phases of {}, by wall time; each bar adds up to its wall time.</p>",
        shown.len(),
        rows.len()
    );
    _ = writeln!(
        out,
        "<svg width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">"
    );
    for (i, row) in shown.iter().enumerate() {
        let y = i as f64 * BAR;
        let mut x = LABEL;
        _ = write!(
            out,
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"end\" fill=\"currentColor\">",
            LABEL - 6.0,
            y + BAR - 3.0
        );
        escape(&row.key, out);
        out.push_str("</text>\n");
        for (ns, color, name) in segments(row) {
            if ns <= 0 {
                continue;
            }
            let w = ns as f64 / most * WIDTH;
            _ = write!(
                out,
                "<rect x=\"{x:.2}\" y=\"{y:.1}\" width=\"{w:.2}\" height=\"{:.1}\" fill=\"{color}\"><title>",
                BAR - 2.0
            );
            let mut tip = String::new();
            _ = write!(
                tip,
                "{}: {name} {:.1} s of {:.1} s",
                row.key,
                ns as f64 / 1e9,
                row.wall_ns as f64 / 1e9
            );
            escape(&tip, out);
            out.push_str("</title></rect>\n");
            x += w;
        }
    }
    out.push_str("</svg>\n");
}

/// A row's time in the bars' order: covered and uncovered running, then each kind of waiting.
fn segments(row: &Row) -> [(i64, &'static str, &'static str); 6] {
    let Totals { running_ns, parked_ns, queued_ns, parked_or_queued_ns, unrecorded_ns } =
        row.totals;
    [
        (running_ns - row.uncovered_ns, COVERED, "running under a child span"),
        (row.uncovered_ns, UNCOVERED, "running, uncovered"),
        (parked_ns, PARKED, "parked"),
        (queued_ns, QUEUED, "queued"),
        (parked_or_queued_ns, PARKED_OR_QUEUED, "parked or queued"),
        (unrecorded_ns, UNRECORDED, "unrecorded"),
    ]
}
