//! `blaze explain memory`, answered from the history's Parquet without DuckDB: its six parts
//! (the views `memory_timeline`, `memory_peak_tasks`, `memory_at_peak`, `memory_held`,
//! `memory_sites` and `memory_give_backs` of `sql/examples.sql`), each a filter, a group and a
//! sum over one or two tables, read through [`crate::tables`] and aggregated here. The SQL of
//! each view stays the definition `blaze debug sql` and the explorer run, and the oracle this
//! module's test holds each answer to, cell for cell, on one fixture (rule one-concept-one-home:
//! the command runs this path only).
//!
//! Each answer has the view's columns, names and types (`DOUBLE` a `Float64`, `BIGINT` an
//! `Int64`, `VARCHAR` a `Utf8`), and its numbers are made as DuckDB makes them: an integer is
//! divided as a double, `round(x, n)` is `round(x * 10^n) / 10^n` with ties away from zero, a
//! cast of a double to `BIGINT` rounds ties to even, and `100.0 * a / b` is a `DECIMAL(38,1)`
//! cast to a double before the division.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};

use crate::{
    history::{Pin, manifest::Scope},
    schema,
    tables::{self, Cells, Rows, TableError},
};

/// The views' `limit` when none is given (`views.rs`'s `LIMIT`, held to it by a test).
pub const DEFAULT_LIMIT: u64 = 20;

/// A part of `blaze explain memory`: one view each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Timeline,
    PeakTasks,
    AtPeak,
    Held,
    Sites,
    GiveBacks,
}

impl Part {
    /// In the order the command prints them.
    pub const ALL: [Part; 6] =
        [Part::Timeline, Part::PeakTasks, Part::AtPeak, Part::Held, Part::Sites, Part::GiveBacks];

    /// The view of `sql/examples.sql` it answers.
    pub const fn view(self) -> &'static str {
        match self {
            Part::Timeline => "memory_timeline",
            Part::PeakTasks => "memory_peak_tasks",
            Part::AtPeak => "memory_at_peak",
            Part::Held => "memory_held",
            Part::Sites => "memory_sites",
            Part::GiveBacks => "memory_give_backs",
        }
    }

    /// Whether its view takes a `limit`.
    pub const fn limited(self) -> bool {
        !matches!(self, Part::Timeline | Part::AtPeak)
    }
}

// --- The rows read ----------------------------------------------------------------------

/// A row of the `builds` view: one a build and start, its end's where it has one.
#[derive(Clone, Debug, PartialEq)]
pub struct Build {
    pub build_id: u64,
    pub started_ns: i64,
    pub dur_ns: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Counter {
    pub build_id: Option<u64>,
    pub process: Option<u32>,
    pub name: String,
    pub t_ns: i64,
    pub value: f64,
}

/// A `spans` row of the kinds the parts read: `task`, the unit phases and the give-backs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub build_id: Option<u64>,
    pub process: Option<u32>,
    pub span_id: Option<u64>,
    pub parent: Option<u64>,
    pub name: String,
    pub function: Option<String>,
    pub krate: Option<String>,
    pub unit: Option<String>,
    pub start_ns: i64,
    pub dur_ns: i64,
    pub peak_bytes: Option<u64>,
    pub granted_bytes: Option<u64>,
    pub heap_allocated_bytes: Option<u64>,
    pub heap_freed_bytes: Option<u64>,
    pub fields: Option<String>,
}

/// A row of `alloc_samples`, or of `held_samples` with its reading's `read_ns`.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub build_id: Option<u64>,
    pub process: Option<u32>,
    pub t_ns: i64,
    pub bytes: u64,
    pub tag: Option<String>,
    pub block_bytes: u64,
    pub site: Option<String>,
    pub read_ns: i64,
}

/// The names of the `task` span's unit phases, as `memory_peak_tasks` reads them.
const PHASES: [&str; 5] = ["unit names", "unit bodies", "unit codegen", "unit link", "unit store"];
/// The spans `memory_give_backs` lists.
const GIVE_BACKS: [&str; 4] =
    ["memory give-back", "memory give-back plan", "memory purge", "kept front ends let go for memory"];
/// The process's own areas `memory_at_peak` lists.
const PROCESS_AREAS: [&str; 10] = [
    "footprint bytes",
    "heap live bytes",
    "heap committed bytes",
    "allocator kept bytes",
    "blaze-mem resident bytes",
    "chunks warm bytes",
    "chunks cold bytes",
    "blocks live bytes",
    "blocks warm bytes",
    "blocks cold bytes",
];

fn cells<'a, const N: usize>(
    rows: &'a Rows,
    names: &[&'static str; N],
) -> Result<[Cells<'a>; N], TableError> {
    let mut out: [Cells<'a>; N] = std::array::from_fn(|_| Cells::Missing);
    for (at, name) in names.iter().enumerate() {
        out[at] = rows.cells(at, name)?;
    }
    Ok(out)
}

fn process_of(cells: &Cells<'_>, row: usize) -> Option<u32> {
    cells.u64(row).and_then(|it| u32::try_from(it).ok())
}

fn owned(text: Option<&str>) -> Option<String> {
    text.map(str::to_owned) // alloc: a kept row's text
}

/// The `builds` view's rows of `scope`: one a build and start, the one with the longest
/// `dur_ns` (an end's over a start's, which has none).
fn builds(dir: &Path, pin: &Pin, scope: Scope) -> Result<Vec<Build>, TableError> {
    const C: [&str; 3] = ["build_id", "started_ns", "dur_ns"];
    // alloc: a build's row, once a build the question reads
    let mut by: BTreeMap<(u64, i64), Option<i64>> = BTreeMap::new();
    for rows in tables::read(dir, pin, &schema::BUILDS, scope, &C)? {
        let [build, started, dur] = cells(&rows, &C)?;
        for row in 0..rows.len() {
            let (Some(build), Some(started)) = (build.u64(row), started.i64(row)) else { continue };
            let dur = dur.i64(row);
            let kept = by.entry((build, started)).or_insert(dur);
            if dur.is_some() && (kept.is_none() || dur > *kept) {
                *kept = dur;
            }
        }
    }
    Ok(by
        .into_iter()
        .map(|((build_id, started_ns), dur_ns)| Build { build_id, started_ns, dur_ns })
        .collect()) // alloc: the builds read, once a question
}

fn counters(dir: &Path, pin: &Pin, scope: Scope) -> Result<Vec<Counter>, TableError> {
    const C: [&str; 5] = ["build_id", "process", "name", "t_ns", "value"];
    // alloc: the counters of the build asked of, once a question
    let mut out = Vec::new();
    for rows in tables::read(dir, pin, &schema::COUNTERS, scope, &C)? {
        let [build, process, name, t, value] = cells(&rows, &C)?;
        for row in 0..rows.len() {
            let (Some(name), Some(t_ns), Some(value)) = (name.text(row), t.i64(row), value.f64(row))
            else {
                continue;
            };
            out.push(Counter {
                build_id: build.u64(row),
                process: process_of(&process, row),
                name: name.to_owned(), // alloc: a kept row's name
                t_ns,
                value,
            });
        }
    }
    Ok(out)
}

/// The `spans` rows of `scope` whose name is one the parts read.
fn spans(dir: &Path, pin: &Pin, scope: Scope) -> Result<Vec<Span>, TableError> {
    const C: [&str; 15] = [
        "build_id",
        "process",
        "span_id",
        "parent",
        "name",
        "function",
        "crate",
        "unit",
        "start_ns",
        "dur_ns",
        "peak_bytes",
        "granted_bytes",
        "heap_allocated_bytes",
        "heap_freed_bytes",
        "fields",
    ];
    // alloc: the spans the parts read, once a question
    let mut out = Vec::new();
    for rows in tables::read(dir, pin, &schema::SPANS, scope, &C)? {
        let [
            build,
            process,
            span_id,
            parent,
            name,
            function,
            krate,
            unit,
            start,
            dur,
            peak,
            granted,
            allocated,
            freed,
            fields,
        ] = cells(&rows, &C)?;
        for row in 0..rows.len() {
            let Some(name) = name.text(row) else { continue };
            if name != "task" && !PHASES.contains(&name) && !GIVE_BACKS.contains(&name) {
                continue;
            }
            let (Some(start_ns), Some(dur_ns)) = (start.i64(row), dur.i64(row)) else { continue };
            out.push(Span {
                build_id: build.u64(row),
                process: process_of(&process, row),
                span_id: span_id.u64(row),
                parent: parent.u64(row),
                name: name.to_owned(), // alloc: a kept row's name
                function: owned(function.text(row)),
                krate: owned(krate.text(row)),
                unit: owned(unit.text(row)),
                start_ns,
                dur_ns,
                peak_bytes: peak.u64(row),
                granted_bytes: granted.u64(row),
                heap_allocated_bytes: allocated.u64(row),
                heap_freed_bytes: freed.u64(row),
                fields: owned(fields.text(row)),
            });
        }
    }
    Ok(out)
}

/// The samples of `table` (`alloc_samples` or `held_samples`) of `scope` that `keep` keeps,
/// given a row's build, process and `read_ns` (0 for `alloc_samples`, which has none).
fn samples(
    dir: &Path,
    pin: &Pin,
    table: &'static schema::Table,
    scope: Scope,
    keep: impl Fn(Option<u64>, Option<u32>, i64) -> bool,
) -> Result<Vec<Sample>, TableError> {
    let held = table.name == schema::HELD_SAMPLES.name;
    let read_ns = if held { "read_ns" } else { "t_ns" };
    let c = ["build_id", "process", "t_ns", "bytes", "tag", "block_bytes", "site", read_ns];
    // alloc: the samples kept, once a question
    let mut out = Vec::new();
    for rows in tables::read(dir, pin, table, scope, &c)? {
        let [build, process, t, bytes, tag, block, site, read] = cells(&rows, &c)?;
        for row in 0..rows.len() {
            let (build_id, process) = (build.u64(row), process_of(&process, row));
            let read_ns = if held { read.i64(row) } else { Some(0) };
            let (Some(t_ns), Some(read_ns)) = (t.i64(row), read_ns) else { continue };
            if !keep(build_id, process, read_ns) {
                continue;
            }
            out.push(Sample {
                build_id,
                process,
                t_ns,
                bytes: bytes.u64(row).unwrap_or(0),
                tag: owned(tag.text(row)),
                block_bytes: block.u64(row).unwrap_or(0),
                site: owned(site.text(row)),
                read_ns,
            });
        }
    }
    Ok(out)
}

// --- The questions ----------------------------------------------------------------------

/// The last build of the history: the greatest `build_id` of `builds`.
pub fn last_build(dir: &Path) -> Result<Option<u64>, TableError> {
    let pin = Pin::take(dir).map_err(TableError::History)?;
    Ok(builds(dir, &pin, Scope::All)?.iter().map(|it| it.build_id).max())
}

/// The last build that has memory rows: a counter, a `task` span's peak or an allocation
/// sample, tagged with it.
pub fn last_with_memory(dir: &Path) -> Result<Option<u64>, TableError> {
    let pin = Pin::take(dir).map_err(TableError::History)?;
    let mut last: Option<u64> = None;
    let mut saw = |it: Option<u64>| last = last.max(it);
    for rows in tables::read(dir, &pin, &schema::COUNTERS, Scope::All, &["build_id"])? {
        let [build] = cells(&rows, &["build_id"])?;
        (0..rows.len()).for_each(|row| saw(build.u64(row)));
    }
    const S: [&str; 3] = ["build_id", "name", "peak_bytes"];
    for rows in tables::read(dir, &pin, &schema::SPANS, Scope::All, &S)? {
        let [build, name, peak] = cells(&rows, &S)?;
        for row in 0..rows.len() {
            if name.text(row) == Some("task") && peak.u64(row).is_some() {
                saw(build.u64(row));
            }
        }
    }
    for rows in tables::read(dir, &pin, &schema::ALLOC_SAMPLES, Scope::All, &["build_id"])? {
        let [build] = cells(&rows, &["build_id"])?;
        (0..rows.len()).for_each(|row| saw(build.u64(row)));
    }
    Ok(last)
}

/// Every part of `blaze explain memory` of `build`, each view's at most `limit` rows where it
/// takes one: its batches (none for no row), in [`Part::ALL`]'s order. The tables are read once,
/// those of one build only as far as the manifest can tell (`Scope::Build`).
pub fn answer(dir: &Path, build: u64, limit: u64) -> Result<Vec<(Part, Vec<RecordBatch>)>, TableError> {
    let pin = Pin::take(dir).map_err(TableError::History)?;
    let scope = Scope::Build(build);
    let facts = Facts {
        builds: builds(dir, &pin, scope)?,
        counters: counters(dir, &pin, scope)?,
        spans: spans(dir, &pin, scope)?,
        alloc: samples(dir, &pin, &schema::ALLOC_SAMPLES, scope, |it, _, _| it == Some(build))?,
    };
    // `held_samples` is not of a build: the reading is the peak process's, its rows any
    // process's of that reading.
    let reading = match held_peak(&facts.counters, build) {
        None => None,
        Some((process, peak)) => {
            let at = samples(dir, &pin, &schema::HELD_SAMPLES, Scope::All, |_, it, _| {
                it == Some(process)
            })?;
            reading_of(at.iter().map(|it| it.read_ns), peak)
        }
    };
    let held = match reading {
        None => Vec::new(), // alloc: none
        Some(read) => {
            samples(dir, &pin, &schema::HELD_SAMPLES, Scope::All, |_, _, it| it == read)?
        }
    };
    // alloc: the six answers, once a question
    let mut out = Vec::with_capacity(Part::ALL.len());
    for part in Part::ALL {
        let batches = match part {
            Part::Timeline => timeline_batch(&timeline(&facts, build)),
            Part::PeakTasks => peak_tasks_batch(&peak_tasks(&facts, build, limit)),
            Part::AtPeak => at_peak_batch(&at_peak(&facts, build)),
            Part::Held => sites_batch(part, &sites(&held, limit), true),
            Part::Sites => sites_batch(part, &sites(&facts.alloc, limit), false),
            Part::GiveBacks => give_backs_batch(&give_backs(&facts, build, limit)),
        }?;
        out.push((part, batches));
    }
    Ok(out)
}

/// What the parts of one build read, but `held_samples`.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub builds: Vec<Build>,
    pub counters: Vec<Counter>,
    pub spans: Vec<Span>,
    /// The build's `alloc_samples`.
    pub alloc: Vec<Sample>,
}

// --- The aggregations, as each view's SQL says them --------------------------------------

/// DuckDB's `round(x, digits)` of a double.
fn round(x: f64, digits: i32) -> f64 {
    let modifier = 10f64.powi(digits);
    let rounded = (x * modifier).round() / modifier;
    if rounded.is_finite() { rounded } else { x }
}

const GIB: f64 = 1_073_741_824.0;

/// `round(x / 1073741824, 2)`.
fn gib(x: f64) -> f64 {
    round(x / GIB, 2)
}

/// DuckDB's cast of a double to `BIGINT`: ties to even.
fn bigint(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(100.0 * part / whole, 1)`: `100.0 * part` a `DECIMAL(38,1)` (its value times ten,
/// an integer) cast to a double, then divided as doubles; null where `whole` is 0.
fn pct(part: u128, whole: u128) -> Option<f64> {
    (whole != 0).then(|| round((part.saturating_mul(1000) as f64 / 10.0) / whole as f64, 1))
}

/// The build's start and end (`started_ns + coalesce(dur_ns, 0)`), as `builds` has them.
fn window(builds: &[Build], build: u64) -> Option<(i64, i64)> {
    let it = builds.iter().find(|it| it.build_id == build)?;
    Some((it.started_ns, it.started_ns + it.dur_ns.unwrap_or(0)))
}

/// The views' `c`: the build's counters, or, where none is tagged with it, the counters of no
/// build in its window (bounds included).
fn counters_of<'a>(facts: &'a Facts, build: u64) -> Vec<&'a Counter> {
    let tagged: Vec<&Counter> =
        facts.counters.iter().filter(|it| it.build_id == Some(build)).collect(); // alloc: a build's counters
    if !tagged.is_empty() {
        return tagged;
    }
    let Some((t0, t1)) = window(&facts.builds, build) else { return tagged };
    facts
        .counters
        .iter()
        .filter(|it| it.build_id.is_none() && t0 <= it.t_ns && it.t_ns <= t1)
        .collect() // alloc: a build's counters
}

/// A row of `memory_timeline`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Minute {
    pub minute: i64,
    pub footprint: Option<f64>,
    pub heap_live: Option<f64>,
    pub blaze_mem: Option<f64>,
    pub kept: Option<f64>,
    pub running: Option<f64>,
}

fn most(into: &mut Option<f64>, value: f64) {
    if into.is_none_or(|it| value > it) {
        *into = Some(value);
    }
}

/// The latest of `points` (sorted by instant) at or before `t`: an `ASOF JOIN ... ON t >= at`.
fn as_of(points: &[(i64, f64)], t: i64) -> Option<f64> {
    let after = points.partition_point(|(at, _)| *at <= t);
    after.checked_sub(1).map(|it| points[it].1)
}

/// `memory_timeline`.
pub fn timeline(facts: &Facts, build: u64) -> Vec<Minute> {
    let c = counters_of(facts, build);
    let Some(first) = c.iter().map(|it| it.t_ns).min() else { return Vec::new() };
    // alloc: a series of the build's counters, once a question
    let series = |name: &str| -> Vec<(i64, f64)> {
        let mut it: Vec<(i64, f64)> =
            c.iter().filter(|it| it.name == name).map(|it| (it.t_ns, it.value)).collect();
        it.sort_by_key(|(t, _)| *t);
        it
    };
    let measured = series("allocator kept bytes");
    let kept = match measured.is_empty() {
        false => measured,
        true => {
            let (heap, footprint) = (series("heap live bytes"), series("footprint bytes"));
            series("blaze-mem resident bytes")
                .into_iter()
                .filter_map(|(t, resident)| {
                    Some((t, as_of(&footprint, t)? - as_of(&heap, t)? - resident))
                })
                .collect() // alloc: as above
        }
    };
    let minute = |t: i64| bigint(((t - first) as f64 / 60e9).floor());
    // alloc: the minutes, once a question
    let mut minutes: BTreeMap<i64, Minute> = BTreeMap::new();
    fn at(minutes: &mut BTreeMap<i64, Minute>, m: i64) -> &mut Minute {
        minutes.entry(m).or_insert_with(|| Minute { minute: m, ..Minute::default() })
    }
    for it in &c {
        let field: fn(&mut Minute) -> &mut Option<f64> = match it.name.as_str() {
            "footprint bytes" => |it| &mut it.footprint,
            "heap live bytes" => |it| &mut it.heap_live,
            "live bytes" => |it| &mut it.blaze_mem,
            "pool running" => |it| &mut it.running,
            _ => continue,
        };
        most(field(at(&mut minutes, minute(it.t_ns))), it.value);
    }
    for (t, value) in kept {
        most(&mut at(&mut minutes, minute(t)).kept, value);
    }
    minutes.into_values().collect() // alloc: the answer's rows
}

/// A row of `memory_peak_tasks`.
#[derive(Clone, Debug, PartialEq)]
pub struct PeakTask<'a> {
    pub task: Option<&'a str>,
    pub krate: Option<&'a str>,
    pub unit: Option<&'a str>,
    pub peak: u64,
    pub granted: Option<u64>,
    pub allocated: Option<u64>,
    pub freed: Option<u64>,
    pub dur_ns: i64,
}

/// `memory_peak_tasks`.
pub fn peak_tasks(facts: &Facts, build: u64, limit: u64) -> Vec<PeakTask<'_>> {
    let of_build = || facts.spans.iter().filter(move |it| it.build_id == Some(build));
    // The phases under each span: their first crate and unit said (`any_value`).
    // alloc: the build's phases, once a question
    // A null process or parent joins nothing, as SQL's `=` has it.
    let mut phases: BTreeMap<(u32, u64), (Option<&str>, Option<&str>)> = BTreeMap::new();
    for it in of_build().filter(|it| PHASES.contains(&it.name.as_str())) {
        let (Some(process), Some(parent)) = (it.process, it.parent) else { continue };
        let (krate, unit) = phases.entry((process, parent)).or_insert((None, None));
        *krate = krate.or(it.krate.as_deref());
        *unit = unit.or(it.unit.as_deref());
    }
    let mut out: Vec<PeakTask<'_>> = of_build()
        .filter(|it| it.name == "task")
        .filter_map(|it| {
            let peak = it.peak_bytes?;
            let phase = it.process.zip(it.span_id).and_then(|key| phases.get(&key)).copied();
            let (krate, unit) = phase.unwrap_or((None, None));
            Some(PeakTask {
                task: it.function.as_deref(),
                krate: it.krate.as_deref().or(krate),
                unit: it.unit.as_deref().or(unit),
                peak,
                granted: it.granted_bytes,
                allocated: it.heap_allocated_bytes,
                freed: it.heap_freed_bytes,
                dur_ns: it.dur_ns,
            })
        })
        .collect(); // alloc: the answer's rows
    // `ORDER BY t.peak_bytes DESC, task`: a null task last.
    out.sort_by(|a, b| {
        b.peak.cmp(&a.peak).then_with(|| match (a.task, b.task) {
            (Some(a), Some(b)) => a.cmp(b),
            (a, b) => a.is_none().cmp(&b.is_none()),
        })
    });
    out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    out
}

/// A row of `memory_at_peak`: a tag's (`process` false) or one of the process's own areas.
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    pub area: String,
    pub process: bool,
    pub held: Option<f64>,
    pub blaze_mem: Option<f64>,
    pub heap: Option<f64>,
    pub allocated: Option<f64>,
    pub freed: Option<f64>,
}

/// `memory_at_peak`.
pub fn at_peak(facts: &Facts, build: u64) -> Vec<Area> {
    let c = counters_of(facts, build);
    // The footprint's peak: its largest value, the earliest of equals.
    let peak = c
        .iter()
        .filter(|it| it.name == "footprint bytes")
        .min_by(|a, b| b.value.total_cmp(&a.value).then(a.t_ns.cmp(&b.t_ns)));
    let Some(peak) = peak.map(|it| it.t_ns) else { return Vec::new() };
    // Each counter's value at its last instant at or before the peak, and at its first.
    // alloc: the counters' values, once a question
    let mut then: BTreeMap<&str, (i64, f64)> = BTreeMap::new();
    let mut first: BTreeMap<&str, (i64, f64)> = BTreeMap::new();
    for it in &c {
        let at = first.entry(it.name.as_str()).or_insert((it.t_ns, it.value));
        if it.t_ns < at.0 {
            *at = (it.t_ns, it.value);
        }
        if it.t_ns <= peak {
            let at = then.entry(it.name.as_str()).or_insert((it.t_ns, it.value));
            if it.t_ns > at.0 {
                *at = (it.t_ns, it.value);
            }
        }
    }
    #[derive(Default)]
    struct Tag {
        mem: Option<f64>,
        heap: Option<f64>,
        allocated: Option<f64>,
        freed: Option<f64>,
    }
    // alloc: the tags, once a question
    let mut tags: BTreeMap<&str, Tag> = BTreeMap::new();
    for (&name, &(_, value)) in &then {
        // `LIKE 'tag %: %'`, and `split_part(name, ': ', 2)`.
        if !name.strip_prefix("tag ").is_some_and(|it| it.contains(": ")) {
            continue;
        }
        let tag = tags.entry(name.split(": ").nth(1).unwrap_or("")).or_default();
        let since = value - first.get(name).map_or(f64::NAN, |it| it.1);
        if name.starts_with("tag live bytes: ") {
            most(&mut tag.mem, value);
        } else if name.starts_with("tag heap bytes: ") {
            most(&mut tag.heap, value);
        } else if name.starts_with("tag heap allocated bytes: ") {
            most(&mut tag.allocated, since);
        } else if name.starts_with("tag heap freed bytes: ") {
            most(&mut tag.freed, since);
        }
    }
    let mut out: Vec<Area> = tags
        .into_iter()
        .map(|(area, it)| Area {
            area: area.to_owned(), // alloc: a row's area
            process: false,
            held: Some(gib(it.mem.unwrap_or(0.0) + it.heap.unwrap_or(0.0))),
            blaze_mem: it.mem.map(gib),
            heap: it.heap.map(gib),
            allocated: it.allocated.map(gib),
            freed: it.freed.map(gib),
        })
        .collect(); // alloc: the answer's rows
    for (&name, &(_, value)) in then.iter().filter(|(name, _)| PROCESS_AREAS.contains(*name)) {
        out.push(Area {
            area: name.replace(" bytes", ""), // alloc: a row's area
            process: true,
            held: Some(gib(value)),
            blaze_mem: None,
            heap: None,
            allocated: None,
            freed: None,
        });
    }
    // `ORDER BY kind DESC, held_gib DESC NULLS LAST, area`: the tags first.
    out.sort_by(|a, b| {
        a.process
            .cmp(&b.process)
            .then_with(|| match (a.held, b.held) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (a, b) => a.is_none().cmp(&b.is_none()),
            })
            .then_with(|| a.area.cmp(&b.area))
    });
    out
}

/// `memory_held`'s peak: the process whose `footprint bytes` of the build was largest, and
/// when it was (the earliest instant of that largest value).
fn held_peak(counters: &[Counter], build: u64) -> Option<(u32, i64)> {
    // alloc: a process's peak, once a question
    let mut by: BTreeMap<u32, (f64, i64)> = BTreeMap::new();
    for it in counters.iter().filter(|it| it.build_id == Some(build) && it.name == "footprint bytes") {
        let Some(process) = it.process else { continue };
        let at = by.entry(process).or_insert((it.value, it.t_ns));
        if it.value > at.0 || (it.value == at.0 && it.t_ns < at.1) {
            *at = (it.value, it.t_ns);
        }
    }
    by.into_iter()
        .min_by(|a, b| b.1.0.total_cmp(&a.1.0).then(a.0.cmp(&b.0)))
        .map(|(process, (_, t))| (process, t))
}

/// `memory_held`'s reading: the process's last at or before its peak, or its first where none
/// was that early.
fn reading_of(read: impl Iterator<Item = i64> + Clone, peak: i64) -> Option<i64> {
    read.clone().filter(|it| *it <= peak).max().or_else(|| read.min())
}

/// A row of `memory_held` (`oldest_min` said) or `memory_sites`.
#[derive(Clone, Debug, PartialEq)]
pub struct Site<'a> {
    pub site: &'a str,
    pub gib: f64,
    pub pct: Option<f64>,
    pub tags: String,
    pub samples: i64,
    pub mean_block_bytes: i64,
    pub oldest_min: f64,
}

/// `memory_sites` over a build's `alloc_samples`, or `memory_held` over one reading's
/// `held_samples`: each call site's bytes, share, tags, samples and mean block, most first.
pub fn sites(samples: &[Sample], limit: u64) -> Vec<Site<'_>> {
    #[derive(Default)]
    struct Sum<'a> {
        bytes: u128,
        blocks: u128,
        samples: i64,
        tags: BTreeSet<&'a str>,
        first_t: Option<i64>,
        last_read: Option<i64>,
    }
    let whole: u128 = samples.iter().map(|it| u128::from(it.bytes)).sum();
    // alloc: the sites, once a question
    let mut by: BTreeMap<Option<&str>, Sum<'_>> = BTreeMap::new();
    for it in samples {
        let sum = by.entry(it.site.as_deref()).or_default();
        sum.bytes += u128::from(it.bytes);
        sum.blocks += u128::from(it.block_bytes);
        sum.samples += 1;
        sum.tags.insert(it.tag.as_deref().unwrap_or("outside"));
        sum.first_t = Some(sum.first_t.map_or(it.t_ns, |t| t.min(it.t_ns)));
        sum.last_read = Some(sum.last_read.map_or(it.read_ns, |t| t.max(it.read_ns)));
    }
    let mut out: Vec<Site<'_>> = by
        .into_iter()
        .map(|(site, it)| Site {
            site: site.unwrap_or("(in the allocator)"),
            gib: gib(it.bytes as f64),
            pct: pct(it.bytes, whole),
            tags: it.tags.into_iter().collect::<Vec<_>>().join(", "), // alloc: a row's tags
            samples: it.samples,
            mean_block_bytes: bigint(it.blocks as f64 / it.samples as f64),
            oldest_min: round(
                (it.last_read.unwrap_or(0) - it.first_t.unwrap_or(0)) as f64 / 60e9,
                1,
            ),
        })
        .collect(); // alloc: the answer's rows
    out.sort_by(|a, b| b.gib.total_cmp(&a.gib).then_with(|| a.site.cmp(b.site)));
    out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    out
}

/// `memory_give_backs`: the build's give-back spans and those of no build in its window, in
/// the order they began, each with when from the build's start.
pub fn give_backs(facts: &Facts, build: u64, limit: u64) -> Vec<(f64, &Span)> {
    let Some((t0, t1)) = window(&facts.builds, build) else { return Vec::new() };
    let mut out: Vec<&Span> = facts
        .spans
        .iter()
        .filter(|it| GIVE_BACKS.contains(&it.name.as_str()))
        .filter(|it| match it.build_id {
            Some(it) => it == build,
            None => t0 <= it.start_ns && it.start_ns < t1,
        })
        .collect(); // alloc: the answer's rows
    out.sort_by_key(|it| it.start_ns);
    out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    out.into_iter().map(|it| (round((it.start_ns - t0) as f64 / 1e6, 1), it)).collect() // alloc: as above
}

// --- The answers as the views' batches --------------------------------------------------

fn column(array: impl arrow_array::Array + 'static) -> ArrayRef {
    Arc::new(array) // alloc: an answer's column
}

fn doubles(values: impl IntoIterator<Item = Option<f64>>) -> ArrayRef {
    column(values.into_iter().collect::<Float64Array>())
}

fn bigints(values: impl IntoIterator<Item = Option<i64>>) -> ArrayRef {
    column(values.into_iter().collect::<Int64Array>())
}

fn varchars<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> ArrayRef {
    column(values.into_iter().collect::<StringArray>())
}

/// The view's answer: one batch of `columns`, or none for no row, as DuckDB's answer reads.
fn batches(view: &'static str, columns: Vec<(&'static str, ArrayRef)>) -> Result<Vec<RecordBatch>, TableError> {
    if columns.first().is_none_or(|(_, it)| it.is_empty()) {
        return Ok(Vec::new()); // alloc: none
    }
    let batch = RecordBatch::try_from_iter(columns).map_err(|error| TableError::Answer { view, error })?;
    Ok(vec![batch]) // alloc: the answer
}

fn timeline_batch(rows: &[Minute]) -> Result<Vec<RecordBatch>, TableError> {
    let g = |f: fn(&Minute) -> Option<f64>| doubles(rows.iter().map(|it| f(it).map(gib)));
    batches(
        Part::Timeline.view(),
        // alloc: the answer's columns
        vec![
            ("minute", bigints(rows.iter().map(|it| Some(it.minute)))),
            ("footprint_gib", g(|it| it.footprint)),
            ("heap_live_gib", g(|it| it.heap_live)),
            ("blaze_mem_gib", g(|it| it.blaze_mem)),
            ("kept_gib", g(|it| it.kept)),
            ("tasks_running", bigints(rows.iter().map(|it| it.running.map(bigint)))),
        ],
    )
}

fn peak_tasks_batch(rows: &[PeakTask<'_>]) -> Result<Vec<RecordBatch>, TableError> {
    let g = |f: fn(&PeakTask<'_>) -> Option<u64>| {
        doubles(rows.iter().map(|it| f(it).map(|it| gib(it as f64))))
    };
    let churn = rows.iter().map(|it| {
        let allocated = it.allocated?;
        (it.peak != 0).then(|| round(allocated as f64 / it.peak as f64, 1))
    });
    batches(
        Part::PeakTasks.view(),
        // alloc: the answer's columns
        vec![
            ("task", varchars(rows.iter().map(|it| it.task))),
            ("crate", varchars(rows.iter().map(|it| it.krate))),
            ("unit", varchars(rows.iter().map(|it| it.unit))),
            ("peak_gib", g(|it| Some(it.peak))),
            ("granted_gib", g(|it| it.granted)),
            ("heap_allocated_gib", g(|it| it.allocated)),
            ("heap_freed_gib", g(|it| it.freed)),
            ("churn_x_peak", doubles(churn)),
            ("s", doubles(rows.iter().map(|it| Some(round(it.dur_ns as f64 / 1e9, 1))))),
        ],
    )
}

fn at_peak_batch(rows: &[Area]) -> Result<Vec<RecordBatch>, TableError> {
    let g = |f: fn(&Area) -> Option<f64>| doubles(rows.iter().map(f));
    let kind = |it: &Area| Some(if it.process { "process" } else { "tag" });
    batches(
        Part::AtPeak.view(),
        // alloc: the answer's columns
        vec![
            ("area", varchars(rows.iter().map(|it| Some(it.area.as_str())))),
            ("kind", varchars(rows.iter().map(kind))),
            ("held_gib", g(|it| it.held)),
            ("blaze_mem_gib", g(|it| it.blaze_mem)),
            ("heap_gib", g(|it| it.heap)),
            ("heap_allocated_gib", g(|it| it.allocated)),
            ("heap_freed_gib", g(|it| it.freed)),
        ],
    )
}

fn sites_batch(part: Part, rows: &[Site<'_>], held: bool) -> Result<Vec<RecordBatch>, TableError> {
    // alloc: the answer's columns
    let mut columns = vec![
        ("site", varchars(rows.iter().map(|it| Some(it.site)))),
        (if held { "held_gib" } else { "allocated_gib" }, doubles(rows.iter().map(|it| Some(it.gib)))),
        ("pct", doubles(rows.iter().map(|it| it.pct))),
        ("tags", varchars(rows.iter().map(|it| Some(it.tags.as_str())))),
        ("samples", bigints(rows.iter().map(|it| Some(it.samples)))),
        ("mean_block_bytes", bigints(rows.iter().map(|it| Some(it.mean_block_bytes)))),
    ];
    if held {
        columns.push(("oldest_min", doubles(rows.iter().map(|it| Some(it.oldest_min)))));
    }
    batches(part.view(), columns)
}

fn give_backs_batch(rows: &[(f64, &Span)]) -> Result<Vec<RecordBatch>, TableError> {
    batches(
        Part::GiveBacks.view(),
        // alloc: the answer's columns
        vec![
            ("at_ms", doubles(rows.iter().map(|(at, _)| Some(*at)))),
            ("ms", doubles(rows.iter().map(|(_, it)| Some(round(it.dur_ns as f64 / 1e6, 1))))),
            ("span", varchars(rows.iter().map(|(_, it)| Some(it.name.as_str())))),
            ("said", varchars(rows.iter().map(|(_, it)| it.fields.as_deref()))),
        ],
    )
}

#[cfg(test)]
mod tests;
