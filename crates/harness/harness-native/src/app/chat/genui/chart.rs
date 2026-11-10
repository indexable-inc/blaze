//! A cell's `<Chart>` (den's ChartProps, den/repl/src/ui/types.ts): bars, stacked bars, a line, an area, a scatter, a pie
//! or a donut, drawn with harness's primitives. Bars are layout nodes (a share of the plot's height each, a tip on each);
//! lines, areas, dots, gridlines and slices are `hn.GenShape`s (canvas.rs) over the plot; the axis, the labels and the
//! legend are words in the text inks. Series take the dataviz palette's slots in a fixed order (`hn.GenTone.series1` on),
//! a single series its `tone` (the accent unless said). Colours never come from the data.
use super::canvas::{Rgba2, Shape, ink, put_shape, tip};
use super::*;
use harness_native_ui_text_api::VectorFit;

/// The most series a chart draws (the palette's slots), the most points of all its series, and the most bars.
const MAX_SERIES: usize = 8;
const MAX_POINTS: usize = 2000;
const MAX_BARS: usize = 200;
/// Hover cells over a line: one per point up to this, then points share one.
const MAX_HITS: usize = 120;
/// X labels under each point or bar up to this many; past it a few, spread out.
const MAX_LABELS: usize = 24;
/// Half an axis tick's line: the plot's top and foot are inset by it, so a gridline runs through its tick's middle.
const TICK_HALF: f32 = 7.0;
/// The labels row's height, and the gap above it.
const LABELS_H: f32 = 14.0;
const LABELS_GAP: f32 = 4.0;

/// One series: its name, ink and values (`xy` for a scatter's points).
struct Series {
    name: String,
    ink: Rgba2,
    v: Vec<f64>,
    xy: Vec<[f64; 2]>,
}

/// A number short: `1.2k`, `3.4M`, `0.25`, `12`.
pub(super) fn short(v: f64) -> String {
    let a = v.abs();
    let (x, suffix) = if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e4 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    let s = if x.fract() == 0.0 || x.abs() >= 100.0 {
        format!("{x:.0}")
    } else if x.abs() >= 10.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.2}")
    };
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s };
    format!("{s}{suffix}")
}

/// Round bounds and a step for an axis over `lo..hi`: ticks 1, 2, 2.5 or 5 times a power of ten apart, about four of them.
fn nice(lo: f64, hi: f64) -> (f64, f64, f64) {
    let (lo, hi) = if hi - lo < 1e-12 { (lo - 1.0, hi + 1.0) } else { (lo, hi) };
    let raw = (hi - lo) / 4.0;
    let p = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * p).find(|s| *s >= raw).unwrap_or(10.0 * p);
    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

fn nums(v: &Value) -> Option<Vec<f64>> {
    v.as_array()?.iter().map(|x| x.as_f64().filter(|f| f.is_finite())).collect()
}

fn pairs(v: &Value) -> Option<Vec<[f64; 2]>> {
    v.as_array()?
        .iter()
        .map(|p| {
            let a = p.as_array()?;
            match a[..] {
                [ref x, ref y] => Some([x.as_f64()?, y.as_f64()?]),
                _ => None,
            }
        })
        .collect()
}

fn with_unit(v: f64, unit: &str) -> String {
    if unit.is_empty() { short(v) } else if unit == "%" { format!("{}%", short(v)) } else { format!("{} {unit}", short(v)) }
}

/// A chart's data, read: a pie's slices, or a plot's series.
enum Data {
    Pie(Vec<(String, f64)>),
    Plot(Vec<Series>),
}

/// What is wrong with a `<Chart>`'s props, named, or None (genui.rs draws it in the failed tone instead).
pub(super) fn check(world: WorldRef<'_>, node: &Value) -> Option<String> {
    read(world, node).err()
}

/// Read a `<Chart>`'s data. Err names what is wrong with it.
fn read(world: WorldRef<'_>, node: &Value) -> Result<(String, Vec<String>, String, Data), String> {
    let kind = match words(node, "kind").as_str() {
        "" => "bar".to_owned(),
        k => k.to_owned(),
    };
    let unit = words(node, "unit");
    let labels: Vec<String> = prop(node, "labels")
        .as_array()
        .map(|a| a.iter().map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_owned)).collect())
        .unwrap_or_default();
    let tone = words(node, "tone");
    if kind == "pie" || kind == "donut" || kind == "segments" {
        let slices = slices_of(node, &kind, &labels)?;
        return Ok((kind, labels, unit, Data::Pie(slices)));
    }
    if !["bar", "line", "area", "scatter", "stacked"].contains(&kind.as_str()) {
        return Err(format!("<Chart kind=\"{kind}\">: the kinds are bar, line, area, scatter, stacked, pie, donut, segments"));
    }
    // Its series: `series`, or `data` as one.
    let mut series = Vec::new(); // alloc: this frame's series, laid onto the chart and dropped
    let given = prop(node, "series");
    if let Some(list) = given.as_array() {
        if list.len() > MAX_SERIES {
            return Err(format!("<Chart> has {} series: it draws at most {MAX_SERIES} (fold the rest into one)", list.len()));
        }
        for (i, s) in list.iter().enumerate() {
            let name = s["name"].as_str().map_or_else(|| format!("series {}", i + 1), str::to_owned);
            let t = s["tone"].as_str().map_or_else(|| format!("series{}", i + 1), str::to_owned);
            let ink = ink(world, &t, 1.0)?.ok_or_else(|| format!("<Chart> series \u{201C}{name}\u{201D} has no ink"))?;
            let (v, xy) = if kind == "scatter" {
                (Vec::new(), pairs(&s["data"]).ok_or_else(|| format!("<Chart> series \u{201C}{name}\u{201D}: data is not [x, y] pairs"))?)
            } else {
                (nums(&s["data"]).ok_or_else(|| format!("<Chart> series \u{201C}{name}\u{201D}: data is not a list of numbers"))?, Vec::new())
            };
            series.push(Series { name, ink, v, xy });
        }
    } else if !given.is_null() {
        return Err("<Chart> series is not a list: series={[{name: 'read', data: [1, 2]}]}".to_owned());
    } else {
        let t = if tone.is_empty() { "accent" } else { tone.as_str() };
        let ink = ink(world, t, 1.0)?.ok_or("<Chart> tone none draws nothing")?;
        let data = prop(node, "data");
        let (v, xy) = if kind == "scatter" {
            (Vec::new(), pairs(data).ok_or("<Chart kind=\"scatter\"> data is not [x, y] pairs: data={[[1, 2], [3, 5]]}")?)
        } else {
            (nums(data).ok_or_else(|| format!("<Chart kind=\"{kind}\"> data is not a list of numbers: data={{[3, 1, 4]}}"))?, Vec::new())
        };
        series.push(Series { name: String::new(), ink, v, xy });
    }
    let total: usize = series.iter().map(|s| s.v.len() + s.xy.len()).sum();
    if total == 0 {
        return Err(format!("<Chart kind=\"{kind}\"> has no data"));
    }
    if total > MAX_POINTS {
        return Err(format!("<Chart> has {total} points: it draws at most {MAX_POINTS}"));
    }
    let n = series.iter().map(|s| s.v.len()).max().unwrap_or(0);
    let bars = kind == "bar" || kind == "stacked";
    if bars && n > MAX_BARS {
        return Err(format!("<Chart kind=\"{kind}\"> has {n} bars: it draws at most {MAX_BARS}; a line takes more"));
    }
    if bars && series.iter().any(|s| s.v.iter().any(|x| *x < 0.0)) {
        return Err(format!("<Chart kind=\"{kind}\">: a bar is 0 or more; draw values below 0 as kind=\"line\""));
    }
    Ok((kind, labels, unit, Data::Plot(series)))
}

/// A `<Chart>`, its props read by [`check`] first. Err names what is wrong with its data.
pub(super) fn chart(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) -> Result<(), String> {
    let (kind, labels, unit, data) = read(v.entity().world(), node)?;
    let series = match data {
        Data::Pie(slices) if kind == "segments" => return segments(v, slices, &unit, b),
        Data::Pie(slices) => return pie(v, node, &kind, slices, &unit, b),
        Data::Plot(series) => series,
    };
    let height = points(node, "height").unwrap_or(160.0).clamp(40.0, 800.0);
    let legend = series.len() > 1;
    let parts: Vec<(&str, &str, u8)> = if legend {
        vec![("plot", "hn.GenChartPlot", 0), ("legend", "hn.GenChartLegend", 1)]
    } else {
        vec![("plot", "hn.GenChartPlot", 0)]
    };
    let mut err = None;
    view::for_each_of(v, parts, |pv, part, _| {
        let r = if part == 0 {
            plot(pv, &kind, &series, &labels, &unit, height, node, b)
        } else {
            keys(pv, series.iter().map(|s| (s.name.clone(), s.ink, String::new())).collect(), b);
            Ok(())
        };
        if let Err(e) = r {
            err = Some(e);
        }
    });
    err.map_or(Ok(()), Err)
}

/// What sits in a plot's area, each made from its prefab.
enum Item {
    /// Drawn behind the bars (gridlines) or over them (lines, areas, dots).
    Back(Shape),
    Over(Shape),
    Bar { h: f32, ink: Rgba2, tip: String },
    /// One bar of each series side by side, or stacked (`stack`), at one x.
    Group { stack: bool, h: f32, parts: Vec<(f32, Rgba2)>, tip: String },
    Hit(String),
}

#[allow(clippy::too_many_arguments)]
fn plot(
    v: View<'_>,
    kind: &str,
    series: &[Series],
    labels: &[String],
    unit: &str,
    height: f32,
    node: &Value,
    b: &mut Budget<'_, '_>,
) -> Result<(), String> {
    let world = v.entity().world();
    let size = Size { w: Sizing::GROW, h: Sizing::fixed(height + LABELS_GAP + LABELS_H) };
    if v.entity().try_cloned::<&Size>() != Some(size) {
        v.set(size);
    }
    let n = series.iter().map(|s| s.v.len()).max().unwrap_or(0);
    let scatter = kind == "scatter";
    // The value range.
    let (lo, hi) = if kind == "stacked" {
        let top = (0..n).map(|i| series.iter().map(|s| s.v.get(i).copied().unwrap_or(0.0)).sum::<f64>()).fold(0.0, f64::max);
        (0.0, top)
    } else {
        let all = series.iter().flat_map(|s| s.v.iter().copied().chain(s.xy.iter().map(|p| p[1])));
        let (a, z) = all.fold((f64::MAX, f64::MIN), |(a, z), x| (a.min(x), z.max(x)));
        // Bars and areas stand on 0.
        if kind == "bar" || kind == "area" { (a.min(0.0), z.max(0.0)) } else { (a, z) }
    };
    let (lo, hi, step) = nice(lo, hi);
    let span = (hi - lo).max(1e-12);
    let mut ticks = Vec::new(); // alloc: this frame's ticks of one chart
    let mut t = hi;
    while t >= lo - step * 0.5 && ticks.len() < 12 {
        ticks.push(with_unit(if t.abs() < step * 1e-6 { 0.0 } else { t }, if unit == "%" { "%" } else { "" }));
        t -= step;
    }
    let faint = ink(world, "faint", 1.0)?;
    let y = |x: f64| (hi - x) as f32;
    // x of a scatter's points.
    let (xlo, xhi, xstep) = if scatter {
        let (a, z) = series.iter().flat_map(|s| s.xy.iter().map(|p| p[0])).fold((f64::MAX, f64::MIN), |(a, z), x| (a.min(x), z.max(x)));
        nice(a, z)
    } else {
        (0.0, 0.0, 1.0)
    };
    let view = if scatter {
        [xlo as f32, 0.0, (xhi - xlo).max(1e-9) as f32, span as f32]
    } else {
        [-0.5, 0.0, n.max(1) as f32, span as f32]
    };
    let shape = |d: String| {
        let mut s = Shape::new(d, view);
        s.fit = VectorFit::Stretch;
        s.fixed = true;
        s.inset = [0.0, TICK_HALF, 0.0, TICK_HALF];
        s
    };
    let mut items: Vec<Item> = Vec::new(); // alloc: this frame's marks of one chart
    // Gridlines through each tick, the quietest ink.
    let mut grid = String::new();
    let (gx0, gx1) = (view[0], view[0] + view[2]);
    for i in 0..ticks.len() {
        grid.push_str(&format!("M{gx0} {}H{gx1}", y(hi - step * i as f64)));
    }
    let mut g = shape(grid);
    g.stroke = faint;
    g.width = 1.0;
    items.push(Item::Back(g));
    let label = |i: usize| labels.get(i).cloned().unwrap_or_default();
    let tip_of = |i: usize| {
        let head = label(i);
        let vals: Vec<String> = series
            .iter()
            .filter_map(|s| s.v.get(i).map(|x| if s.name.is_empty() { with_unit(*x, unit) } else { format!("{} {}", s.name, with_unit(*x, unit)) }))
            .collect();
        if head.is_empty() { vals.join(" · ") } else { format!("{head}: {}", vals.join(" · ")) }
    };
    let frac = |x: f64| ((x - lo) / span).clamp(0.0, 1.0) as f32;
    match kind {
        "bar" if series.len() == 1 => {
            for (i, x) in series[0].v.iter().enumerate() {
                items.push(Item::Bar { h: frac(*x), ink: series[0].ink, tip: tip_of(i) });
            }
        }
        "bar" | "stacked" => {
            let stack = kind == "stacked";
            for i in 0..n {
                let vals: Vec<(f32, Rgba2)> = series.iter().map(|s| (s.v.get(i).copied().unwrap_or(0.0) as f32, s.ink)).collect();
                let h = if stack { frac(vals.iter().map(|p| f64::from(p.0)).sum()) } else { 1.0 };
                let parts = if stack {
                    // Top first, down a column: the first series sits on the baseline.
                    vals.into_iter().rev().collect()
                } else {
                    vals.into_iter().map(|(x, k)| (frac(f64::from(x)), k)).collect()
                };
                items.push(Item::Group { stack, h, parts, tip: tip_of(i) });
            }
        }
        "line" | "area" => {
            let fo = prop(node, "fillOpacity").as_f64().map_or(if series.len() > 1 { 0.14 } else { 0.22 }, |f| f.clamp(0.0, 1.0)) as f32;
            for s in series {
                if s.v.is_empty() {
                    continue;
                }
                let mut d = String::new();
                for (i, x) in s.v.iter().enumerate() {
                    d.push_str(&format!("{}{i} {}", if i == 0 { 'M' } else { 'L' }, y(*x)));
                }
                if s.v.len() == 1 {
                    d.push_str("h0.002");
                }
                if kind == "area" {
                    let base = y(lo.max(0.0).min(hi));
                    let mut a = format!("M0 {base}");
                    for (i, x) in s.v.iter().enumerate() {
                        a.push_str(&format!("L{i} {}", y(*x)));
                    }
                    a.push_str(&format!("L{} {base}Z", s.v.len() - 1));
                    let mut fill = shape(a);
                    fill.fill = Some(fade(s.ink, fo));
                    items.push(Item::Over(fill));
                }
                let mut line = shape(d);
                line.stroke = Some(s.ink);
                line.width = 2.0;
                items.push(Item::Over(line));
            }
            // A tip for each point (or a run of them), over the whole height.
            let per = n.div_ceil(MAX_HITS).max(1);
            for i in (0..n).step_by(per) {
                items.push(Item::Hit(tip_of(i)));
            }
        }
        _ => {
            // A scatter: a dot at each point, each series its ink.
            let eps = (xhi - xlo).max(1e-9) * 1e-3;
            for s in series {
                let mut d = String::new();
                for p in &s.xy {
                    d.push_str(&format!("M{} {}h{eps}", p[0], y(p[1])));
                }
                let mut dots = shape(d);
                dots.stroke = Some(s.ink);
                dots.width = 7.0;
                items.push(Item::Over(dots));
            }
        }
    }
    let cost = items.len() + ticks.len() + n.min(MAX_LABELS) + 4;
    if cost > b.left {
        b.cut = true;
        return Ok(());
    }
    b.left -= cost;
    if let Some(axis) = v.part("axis") {
        let pad = harness_native_ui_layout_api::Padding { l: 0.0, r: 0.0, t: 0.0, b: LABELS_GAP + LABELS_H };
        if axis.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(pad) {
            axis.set(pad);
        }
        view::for_each_of(axis, ticks.iter().enumerate().map(|(i, t)| (format!("t{i}"), "hn.GenChartTick", t)), |tv, t, _| {
            tv.text(t);
        });
    }
    let Some(body) = v.part("body") else { return Ok(()) };
    if let Some(area) = body.part("area") {
        let pf = |it: &Item| match it {
            Item::Back(_) => "hn.GenShapeBack",
            Item::Over(_) => "hn.GenShape",
            Item::Bar { .. } => "hn.GenChartBar",
            Item::Group { stack: true, .. } => "hn.GenChartStack",
            Item::Group { .. } => "hn.GenChartGroup",
            Item::Hit(_) => "hn.GenChartHit",
        };
        view::for_each_of(area, items.iter().enumerate().map(|(i, it)| (format!("i{i}"), pf(it), it)), |iv, it, _| match it {
            Item::Back(s) | Item::Over(s) => put_shape(iv, s),
            Item::Bar { h, ink, tip: t } => {
                bar(iv, *h, *ink);
                tip(iv, t);
            }
            Item::Group { stack, h, parts, tip: t } => {
                let want = Size { w: Sizing::GROW, h: Sizing { kind: SizeKind::Percent, value: *h, min: 0.0, max: 0.0 } };
                if iv.entity().try_cloned::<&Size>() != Some(want) {
                    iv.set(want);
                }
                tip(iv, t);
                view::for_each_of(iv, parts.iter().enumerate().map(|(i, p)| (format!("p{i}"), "hn.GenChartBar", p)), |pv, &(x, k), _| {
                    if *stack {
                        // A share of the stack, by its value.
                        let want = Size { w: Sizing::GROW, h: Sizing { kind: SizeKind::Grow, value: x.max(1e-6), min: 0.0, max: 0.0 } };
                        if pv.entity().try_cloned::<&Size>() != Some(want) {
                            pv.set(want);
                        }
                        ink_on(pv, k);
                    } else {
                        bar(pv, x, k);
                    }
                });
            }
            Item::Hit(t) => tip(iv, t),
        });
    }
    if let Some(row) = body.part("labels") {
        // Under each bar or point when there are few; else a few spread out (a scatter's x ticks).
        let shown: Vec<String> = if scatter {
            let mut out = Vec::new();
            let mut x = xlo;
            while x <= xhi + xstep * 0.5 && out.len() < 12 {
                out.push(short(x));
                x += xstep;
            }
            out
        } else if n <= MAX_LABELS {
            (0..n).map(label).collect()
        } else {
            (0..6).map(|k| label(k * (n - 1) / 5)).collect()
        };
        let spread = scatter || n > MAX_LABELS;
        let mut f = row.entity().try_cloned::<&harness_native_ui_layout_api::Flex>().unwrap_or_default();
        let j = if spread { harness_native_ui_layout_api::Justify::SpaceBetween } else { harness_native_ui_layout_api::Justify::Start };
        if f.justify != j {
            f.justify = j;
            row.set(f);
        }
        view::for_each_of(row, shown.iter().enumerate().map(|(i, s)| (format!("x{i}"), "hn.GenChartLabel", s)), |lv, s, _| {
            let want = if spread { Size { w: Sizing::FIT, h: Sizing::GROW } } else { Size { w: Sizing::GROW, h: Sizing::GROW } };
            if lv.entity().try_cloned::<&Size>() != Some(want) {
                lv.set(want);
            }
            if let Some(t) = lv.part("text") {
                t.text(s);
            }
        });
    }
    Ok(())
}

fn fade(k: Rgba2, a: f32) -> Rgba2 {
    let f = |c: harness_native_ui_look_api::Rgba| harness_native_ui_look_api::Rgba { a: c.a * a, ..c };
    Rgba2 { dark: f(k.dark), light: f(k.light) }
}

fn ink_on(v: View<'_>, k: Rgba2) {
    let want = Fill { dark: k.dark, light: k.light };
    if v.entity().try_cloned::<&Fill>() != Some(want) {
        v.set(want);
    }
}

/// A bar `h` of its cell's height, in `k`.
fn bar(v: View<'_>, h: f32, k: Rgba2) {
    let want = Size { w: Sizing::GROW, h: Sizing { kind: SizeKind::Percent, value: h, min: 0.0, max: 0.0 } };
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
    ink_on(v, k);
}

/// A legend: a swatch, a name and a quiet value for each entry.
fn keys(v: View<'_>, entries: Vec<(String, Rgba2, String)>, b: &mut Budget<'_, '_>) {
    let entries: Vec<_> = entries.into_iter().take(b.left).collect();
    b.left -= entries.len();
    view::for_each_of(v, entries.iter().enumerate().map(|(i, e)| (format!("k{i}"), "hn.GenChartKey", e)), |kv, (name, k, value), _| {
        if let Some(s) = kv.part("swatch") {
            ink_on(s, *k);
        }
        if let Some(t) = kv.part("label") {
            t.text(name);
        }
        if let Some(t) = kv.part("value") {
            t.text(value);
            // No value: it takes no room.
            let none = Size { w: Sizing::fixed(0.0), h: Sizing::FIT };
            if value.is_empty() && t.entity().try_cloned::<&Size>() != Some(none) {
                t.set(none);
            } else if !value.is_empty() && t.entity().has(Size::id()) {
                t.entity().remove(Size::id());
            }
        }
    });
}

/// Parts of a whole as one bar (a disk's volumes, a budget's lines): each part a segment as long as its share, in a
/// series slot's ink (one called `other...` or `free...` in the quiet ink), and under it a legend, each part's swatch,
/// name and value.
fn segments(v: View<'_>, slices: Vec<(String, f64)>, unit: &str, b: &mut Budget<'_, '_>) -> Result<(), String> {
    let world = v.entity().world();
    let total: f64 = slices.iter().map(|s| s.1).sum();
    if !(total > 0.0) {
        return Err("<Chart kind=\"segments\"> has nothing to show: its values sum to 0".to_owned());
    }
    if slices.len() * 2 + 4 > b.left {
        b.cut = true;
        return Ok(());
    }
    let quiet = |l: &str| ["other", "free", "unused", "left"].iter().any(|q| l.to_lowercase().starts_with(q));
    // What is left (free, other) goes last, whatever its size: the bar reads as what is taken, then what is not.
    let mut slices = slices;
    slices.sort_by_key(|(l, _)| quiet(l));
    let mut parts = Vec::new(); // alloc: this frame's segments of one bar
    let mut slot = 0;
    for (label, value) in &slices {
        let k = if quiet(label) {
            ink(world, "muted", 0.35)?
        } else {
            slot += 1;
            ink(world, &format!("series{}", (slot - 1) % 8 + 1), 1.0)?
        };
        parts.push((label.clone(), *value, k.ok_or("a segment has no ink")?));
    }
    b.left -= parts.len();
    if let Some(bar) = v.part("bar") {
        view::for_each_of(bar, parts.iter().enumerate().map(|(i, p)| (format!("s{i}"), "hn.GenSegment", p)), |sv, (_, value, k), _| {
            ink_on(sv, *k);
            // As long as its share; a sliver keeps a hairline of weight so it still shows.
            let w = Size { w: Sizing { value: ((*value / total) as f32).max(0.002), min: 3.0, ..Sizing::GROW }, h: Sizing::GROW };
            if sv.entity().try_cloned::<&Size>() != Some(w) {
                sv.set(w);
            }
        });
    }
    if let Some(legend) = v.part("legend") {
        keys(legend, parts.into_iter().map(|(l, value, k)| (l, k, with_unit(value, unit))).collect(), b);
    }
    Ok(())
}

/// A pie or a donut: its slices largest first (one called `other...` last, in the quiet ink), each a series slot's ink,
/// a legend beside with each slice's value and share, a donut's total (or `center`) in its middle.
/// A pie's slices, largest first, one called `other...` last, past the palette's slots the smallest folded into it.
fn slices_of(node: &Value, kind: &str, labels: &[String]) -> Result<Vec<(String, f64)>, String> {
    let data = prop(node, "data").as_array().ok_or_else(|| format!("<Chart kind=\"{kind}\"> data is not a list: data={{[{{label: 'a', value: 3}}]}}"))?;
    let mut slices: Vec<(String, f64)> = Vec::new(); // alloc: this frame's slices of one pie
    for (i, d) in data.iter().enumerate() {
        let (label, value) = match d {
            Value::Object(o) => (
                o.get("label").and_then(Value::as_str).map_or_else(|| labels.get(i).cloned().unwrap_or_default(), str::to_owned),
                o.get("value").and_then(Value::as_f64),
            ),
            x => (labels.get(i).cloned().unwrap_or_default(), x.as_f64()),
        };
        let value = value.filter(|x| x.is_finite()).ok_or_else(|| format!("<Chart kind=\"{kind}\"> slice {i} has no number value: {{label, value}}"))?;
        if value < 0.0 {
            return Err(format!("<Chart kind=\"{kind}\"> slice \u{201C}{label}\u{201D} is {value}: a slice is 0 or more"));
        }
        slices.push((label, value));
    }
    let total: f64 = slices.iter().map(|s| s.1).sum();
    if slices.is_empty() || total <= 0.0 {
        return Err(format!("<Chart kind=\"{kind}\"> has nothing to share out: its values add up to 0"));
    }
    let other = |l: &str| l.to_lowercase().starts_with("other");
    slices.sort_by(|a, b| other(&a.0).cmp(&other(&b.0)).then(b.1.total_cmp(&a.1)));
    // Past the palette's slots, the smallest fold into one "other".
    if slices.len() > MAX_SERIES {
        let rest: f64 = slices.drain(MAX_SERIES - 1..).map(|s| s.1).sum();
        slices.push(("other".to_owned(), rest));
    }
    Ok(slices)
}

fn pie(v: View<'_>, node: &Value, kind: &str, slices: Vec<(String, f64)>, unit: &str, b: &mut Budget<'_, '_>) -> Result<(), String> {
    let world = v.entity().world();
    let total: f64 = slices.iter().map(|s| s.1).sum();
    let other = |l: &str| l.to_lowercase().starts_with("other");
    let side = points(node, "height").unwrap_or(140.0).clamp(48.0, 480.0);
    let donut = kind == "donut";
    let hole = if donut { prop(node, "hole").as_f64().unwrap_or(0.62).clamp(0.2, 0.9) } else { 0.0 };
    // The gap between slices, in points: the card's ground showing through, as wide at the centre as at the rim.
    let sep = prop(node, "sep").as_f64().unwrap_or(1.0).clamp(0.0, 8.0);
    let mut shapes = Vec::new(); // alloc: this frame's slices as shapes
    let mut keys_in = Vec::new(); // alloc: this frame's legend entries
    let mut a0 = -std::f64::consts::FRAC_PI_2;
    let r_pt = f64::from(side) * 0.5;
    for (i, (label, value)) in slices.iter().enumerate() {
        let k = if other(label) { ink(world, "muted", 0.6)? } else { ink(world, &format!("series{}", i + 1), 1.0)? };
        let k = k.ok_or("a slice has no ink")?;
        let turn = value / total * std::f64::consts::TAU;
        let a1 = a0 + turn;
        // Each straight edge moved in by half of `sep` (`h`, in radii), so a gap is a strip of one width: the rim's ends
        // turn in by the angle that is `h` across, and a pie's point leaves the centre along the middle of its turn.
        let h = if slices.len() > 1 { sep * 0.5 / r_pt } else { 0.0 };
        let g = h.min(0.99).asin().min(turn * 0.25);
        let (s0, s1) = (a0 + g, a1 - g);
        let pt = |r: f64, a: f64| (r * a.cos(), r * a.sin());
        let apex = if h > 0.0 { pt((h / (turn * 0.5).sin().max(1e-6)).min(0.5), a0 + turn * 0.5) } else { (0.0, 0.0) };
        let d = if slices.len() == 1 {
            let o = "M-1 0A1 1 0 1 1 1 0A1 1 0 1 1 -1 0Z".to_owned();
            if donut { format!("{o}M-{hole} 0A{hole} {hole} 0 1 0 {hole} 0A{hole} {hole} 0 1 0 -{hole} 0Z") } else { o }
        } else {
            let large = u8::from(s1 - s0 > std::f64::consts::PI);
            let (ox0, oy0) = pt(1.0, s0);
            let (ox1, oy1) = pt(1.0, s1);
            if donut {
                let gi = (h / hole).min(0.99).asin().min(turn * 0.25);
                let (i0, i1) = (a0 + gi, a1 - gi);
                let (ix0, iy0) = pt(hole, i0);
                let (ix1, iy1) = pt(hole, i1);
                let il = u8::from(i1 - i0 > std::f64::consts::PI);
                format!("M{ox0} {oy0}A1 1 0 {large} 1 {ox1} {oy1}L{ix1} {iy1}A{hole} {hole} 0 {il} 0 {ix0} {iy0}Z")
            } else {
                let (px, py) = apex;
                format!("M{px} {py}L{ox0} {oy0}A1 1 0 {large} 1 {ox1} {oy1}Z")
            }
        };
        let mut s = Shape::new(d, [-1.0, -1.0, 2.0, 2.0]);
        s.fill = Some(k);
        s.even_odd = slices.len() == 1 && donut;
        shapes.push(s);
        let share = value / total * 100.0;
        let said = if unit == "%" { with_unit(*value, unit) } else { format!("{}  {}%", with_unit(*value, unit), short(share.round())) };
        keys_in.push((if label.is_empty() { format!("slice {}", i + 1) } else { label.clone() }, k, said));
        a0 = a1;
    }
    if shapes.len() * 2 + 6 > b.left {
        b.cut = true;
        return Ok(());
    }
    b.left -= shapes.len() + 6;
    if let Some(disc) = v.part("disc") {
        let want = Size { w: Sizing::fixed(side), h: Sizing::fixed(side) };
        if disc.entity().try_cloned::<&Size>() != Some(want) {
            disc.set(want);
        }
        let center = if donut {
            // The total alone, big, its unit (or the caption) under it: the hole is only so wide.
            let big = match prop(node, "center") {
                Value::Null if unit == "%" => with_unit(total, unit),
                Value::Null => short(total),
                c => c.as_str().map_or_else(|| c.to_string(), str::to_owned),
            };
            let caption = words(node, "caption");
            let caption = if caption.is_empty() && unit != "%" { unit.to_string() } else { caption };
            Some((big, caption))
        } else {
            None
        };
        let mut kids: Vec<(String, &str, Option<&Shape>)> =
            shapes.iter().enumerate().map(|(i, s)| (format!("s{i}"), "hn.GenShape", Some(s))).collect();
        if center.is_some() {
            kids.push(("mid".to_owned(), "hn.GenChartCenter", None));
        }
        view::for_each_of(disc, kids, |sv, s, _| match s {
            Some(s) => put_shape(sv, s),
            None => {
                if let Some((big, caption)) = &center {
                    if let Some(t) = sv.part("value") {
                        t.text(big);
                    }
                    if let Some(t) = sv.part("caption") {
                        t.text(caption);
                    }
                }
            }
        });
    }
    if let Some(legend) = v.part("legend") {
        keys(legend, keys_in, b);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_bounds_are_round() {
        assert_eq!(nice(0.0, 97.0), (0.0, 100.0, 25.0));
        assert_eq!(nice(0.0, 1200.0), (0.0, 1500.0, 500.0));
        assert_eq!(nice(-3.0, 7.0), (-5.0, 7.5, 2.5));
        assert_eq!(short(1200.0), "1200");
        assert_eq!(short(12_000.0), "12k");
        assert_eq!(short(2_500_000.0), "2.5M");
        assert_eq!(short(0.25), "0.25");
    }
}
