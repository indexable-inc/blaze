//! The model read as rows: time per unit, per item or per wait, the cores' busy time, and the
//! text `blaze debug time` prints.

use std::fmt::Write as _;

use blaze_hash::HashMap;
use std::alloc::Global;

use super::{
    Model, State, Totals, WAIT_SPAN, Wait,
    path::{Link, LinkKind, Path, Reached},
};

/// What a report's rows are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The phase spans of a unit (`unit names`, `unit codegen`, ...), by phase and unit.
    Unit,
    /// Every span, by its name and what it is about.
    Item,
    /// Every wait, by what it waited for: a key's query, a scope's creator, a counter.
    Wait,
}

impl By {
    pub const NAMES: &'static str = "unit, item or wait";

    pub fn of(name: &str) -> Option<By> {
        match name {
            "unit" => Some(By::Unit),
            "item" => Some(By::Item),
            "wait" => Some(By::Wait),
            _ => None,
        }
    }
}

/// The prefix of a unit's phase spans (`blaze-cargo` serve/unit/compile.rs).
pub const UNIT_PHASE: &str = "unit ";

/// A row of a report: one key's spans, or one kind of wait.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub count: u64,
    /// Their open time all told: spans' `dur_ns`, or waits' park to resume.
    pub wall_ns: i64,
    pub totals: Totals,
    /// Running under no child span.
    pub uncovered_ns: i64,
    /// Waiting under no `wait` span: a wait that says nothing of what it waits for.
    pub unnamed_wait_ns: i64,
    /// For a wait's row: how many of its waits name their waker.
    pub woken_by_known: u64,
}

/// The rows of `model` by `by`, those whose key holds `matching` when given, longest first.
pub fn rows(model: &Model, by: By, matching: Option<&str>) -> Vec<Row> {
    // alloc: a report's rows, once a report
    let mut rows: HashMap<String, Row, Global> = HashMap::new_in(Global);
    match by {
        By::Unit | By::Item => {
            for (i, span) in model.spans.iter().enumerate() {
                if by == By::Unit && !span.name.starts_with(UNIT_PHASE) {
                    continue;
                }
                let Some(it) = row(&mut rows, matching, span.item()) else { continue };
                let time = &model.span_time[i];
                it.count += 1;
                it.wall_ns += span.dur_ns;
                it.totals.add(time.totals);
                it.uncovered_ns += time.uncovered_ns();
                if span.name != WAIT_SPAN {
                    it.unnamed_wait_ns += time.own.waiting_ns();
                }
            }
        }
        By::Wait => {
            // alloc: once a report
            let mut of_wait = vec![Totals::default(); model.waits.len()];
            for lane in &model.lanes {
                for it in &lane.intervals {
                    if let Some(wait) = it.state.wait() {
                        of_wait[wait].add(super::Totals::of(it.state, it.dur_ns()));
                    }
                }
            }
            for (wait, totals) in model.waits.iter().zip(of_wait) {
                let Some(it) = row(&mut rows, matching, wait_class(model, wait)) else { continue };
                it.count += 1;
                it.wall_ns += totals.total();
                it.totals.add(totals);
                it.woken_by_known += u64::from(wait.waker.lane().is_some());
            }
        }
    }
    // alloc: a report's rows, once a report
    let mut rows: Vec<Row> = rows.into_values().collect();
    rows.sort_by(|a, b| b.wall_ns.cmp(&a.wall_ns).then_with(|| a.key.cmp(&b.key)));
    rows
}

/// The row of `key`, unless it does not hold `matching`.
fn row<'a>(
    rows: &'a mut HashMap<String, Row, Global>,
    matching: Option<&str>,
    key: String,
) -> Option<&'a mut Row> {
    if matching.is_some_and(|it| !key.contains(it)) {
        return None;
    }
    // clone: a row's key, once a row
    Some(rows.entry(key.clone()).or_insert_with(|| Row { key, ..Row::default() }))
}

/// What kind of thing a wait waited for, for a report's row: a scope's creator, a key's
/// query, else its label.
pub fn wait_class(model: &Model, wait: &Wait) -> String {
    let kind = wait.on.map_or("span", |it| it.name());
    // A wait whose span the record lacks is named by its kind alone: its value (a key's raw
    // number, `key key 0000…`) says nothing a reader can act on, and one class per value would
    // bury the rest.
    if wait.span.and_then(|it| model.spans[it].awaits.as_deref()).is_none() {
        // alloc: a row's key
        return format!("{kind} wait with no wait span");
    }
    let label = model.wait_label(wait);
    if let Some(rest) = label.strip_prefix("scope ")
        && let Some((_, creator)) = rest.split_once(" of ")
    {
        // alloc: a row's key
        return format!("scope of {creator}");
    }
    match label.split_once('(') {
        // alloc: a row's key
        Some((query, _)) if !query.contains(' ') => format!("{kind} {query}"),
        // alloc: a row's key
        _ => format!("{kind} {label}"),
    }
}

/// A core's running time in one bucket of the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreBusy {
    pub process: u32,
    pub core: u32,
    pub t0_ns: i64,
    pub t1_ns: i64,
    pub running_ns: i64,
}

/// Each core's running time per bucket of `width_ns`, every core and bucket (zeros too), by
/// core then time.
pub fn core_busy(model: &Model, width_ns: i64) -> Vec<CoreBusy> {
    let width = width_ns.max(1);
    let (start, end) = (model.bounds.start_ns, model.bounds.end_ns);
    let buckets = usize::try_from((end - start + width - 1) / width).unwrap_or(0).max(1);
    // A BTreeMap: the rows are in core order.
    // alloc: once a report
    let mut cores: std::collections::BTreeMap<(u32, u32), Vec<i64>> =
        std::collections::BTreeMap::new();
    for lane in &model.lanes {
        for it in &lane.intervals {
            let State::Running { core: Some(core), .. } = it.state else { continue };
            // alloc: a core's buckets, once a core a report
            let busy = cores.entry((lane.id.process(), core)).or_insert_with(|| vec![0; buckets]);
            let (mut from, to) = (it.from_ns.max(start), it.to_ns.min(end));
            while from < to {
                let b = usize::try_from((from - start) / width).unwrap_or(0).min(buckets - 1);
                let bucket_end = start + (b as i64 + 1) * width;
                let upto = to.min(bucket_end);
                busy[b] += upto - from;
                from = upto;
            }
        }
    }
    // alloc: once a report
    let mut out = Vec::with_capacity(cores.len() * buckets);
    for ((process, core), busy) in cores {
        for (b, running_ns) in busy.into_iter().enumerate() {
            let t0_ns = start + b as i64 * width;
            out.push(CoreBusy {
                process,
                core,
                t0_ns,
                t1_ns: (t0_ns + width).min(end),
                running_ns,
            });
        }
    }
    out
}

/// How busy the cores were over the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utilization {
    /// Cores any job ran on.
    pub cores: u32,
    pub wall_ns: i64,
    pub running_ns: i64,
}

impl Utilization {
    /// Running over cores x wall, from 0 to 1.
    pub fn ratio(&self) -> f64 {
        let capacity = f64::from(self.cores) * self.wall_ns as f64;
        if capacity > 0.0 { self.running_ns as f64 / capacity } else { 0.0 }
    }
}

pub fn utilization(model: &Model) -> Utilization {
    let mut cores = blaze_hash::HashSet::new_in(Global);
    let mut running_ns = 0;
    let (start, end) = (model.bounds.start_ns, model.bounds.end_ns);
    for lane in &model.lanes {
        for it in &lane.intervals {
            if let State::Running { core: Some(core), .. } = it.state {
                cores.insert((lane.id.process(), core));
                running_ns += (it.to_ns.min(end) - it.from_ns.max(start)).max(0);
            }
        }
    }
    Utilization { cores: cores.len() as u32, wall_ns: model.bounds.wall_ns(), running_ns }
}

/// Every job lane's totals, added.
pub fn job_totals(model: &Model) -> Totals {
    let mut it = Totals::default();
    for lane in model.lanes.iter().filter(|it| it.id.job().is_some()) {
        it.add(lane.totals());
    }
    it
}

/// What a link of the path is about: the span it ran inside, or what it waited for.
pub fn link_item(model: &Model, link: &Link) -> String {
    let interval = &model.lanes[link.lane].intervals[link.interval];
    match (link.kind, interval.state.wait()) {
        (LinkKind::Waited | LinkKind::Wake, Some(wait)) => model.wait_label(&model.waits[wait]),
        // alloc: a label
        (LinkKind::Queued, _) => "queued for a core".to_owned(),
        // alloc: a label
        (LinkKind::Spawned, _) => "queued for a core after its spawner ended".to_owned(),
        (LinkKind::Unrecorded, _) => match interval.state {
            // alloc: a label
            State::Unrecorded { why } => format!("the record lost it ({})", why.name()),
            // alloc: a label
            _ => "the record lost it".to_owned(),
        },
        _ => match interval.inside {
            Some(span) => {
                let root = model.root_of(span);
                let inner = model.spans[span].item();
                match root == span {
                    true => inner,
                    // alloc: a label
                    false => format!("{inner} in {}", model.spans[root].item()),
                }
            }
            // alloc: a label
            None => "inside no span".to_owned(),
        },
    }
}

/// Seconds, to a tenth.
fn s(ns: i64) -> f64 {
    ns as f64 / 1e9
}

fn pct(part: i64, whole: i64) -> f64 {
    if whole > 0 { 100.0 * part as f64 / whole as f64 } else { 0.0 }
}

/// The summary lines: the build's wall time, its cores, and its jobs' time by state.
pub fn summary(model: &Model, out: &mut String) {
    let util = utilization(model);
    let jobs = job_totals(model);
    let bounds = match model.bounds.from {
        super::BoundsFrom::Build => "its builds row",
        super::BoundsFrom::Record => "its first and last rows (no builds row with a duration)",
    };
    // Writing to a `String` does not fail.
    _ = writeln!(
        out,
        "build {}: {:.1} s wall, from {bounds}",
        model.build,
        s(model.bounds.wall_ns())
    );
    _ = writeln!(
        out,
        "cores: {} ran jobs, busy {:.1}% on average ({:.1} s running of {:.1} s cores x wall)",
        util.cores,
        100.0 * util.ratio(),
        s(util.running_ns),
        s(i64::from(util.cores) * util.wall_ns)
    );
    _ = writeln!(
        out,
        "jobs: {} lanes, {:.1} s lifetime: running {:.1} s, parked {:.1} s, queued {:.1} s, parked or queued {:.1} s, unrecorded {:.1} s",
        model.lanes.len(),
        s(jobs.total()),
        s(jobs.running_ns),
        s(jobs.parked_ns),
        s(jobs.queued_ns),
        s(jobs.parked_or_queued_ns),
        s(jobs.unrecorded_ns)
    );
    if jobs.parked_ns == 0 && jobs.queued_ns == 0 && jobs.parked_or_queued_ns > 0 {
        _ = writeln!(out, "  the record has no `ready` rows, so parked and queued are one column");
    }
    let a = model.anomalies;
    if a != super::Anomalies::default() {
        _ = writeln!(
            out,
            "  rows that did not line up: {} spans with no parent row, {} spans past their parent or a sibling, {} runs cut by another job's switch, {} job spans naming a job with no switch rows, {} jobs given by no job whose creator span has no row",
            a.orphans, a.misnested, a.runs_cut, a.spans_of_no_lane, a.creators_unread
        );
    }
}

/// `rows` as a table, the first `top`, then their sum.
pub fn table(rows: &[Row], by: By, top: usize, out: &mut String) {
    let wait = by == By::Wait;
    _ = writeln!(
        out,
        "{:>6} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}  {}",
        "count",
        "wall s",
        "running",
        "parked",
        "queued",
        "park|que",
        if wait { "unrec" } else { "uncover" },
        if wait { "wakers" } else { "unnamed" },
        match by {
            By::Unit => "phase and unit",
            By::Item => "span and what it is of",
            By::Wait => "what it waited for",
        }
    );
    let mut sum = Row::default();
    for (i, it) in rows.iter().enumerate() {
        sum.count += it.count;
        sum.wall_ns += it.wall_ns;
        sum.totals.add(it.totals);
        sum.uncovered_ns += it.uncovered_ns;
        sum.unnamed_wait_ns += it.unnamed_wait_ns;
        sum.woken_by_known += it.woken_by_known;
        if i < top {
            line(it, wait, &it.key, out);
        }
    }
    if rows.len() > top {
        _ = writeln!(out, "  ... {} more rows", rows.len() - top);
    }
    // alloc: a label
    line(&sum, wait, &format!("all {} rows", rows.len()), out);
}

fn line(it: &Row, wait: bool, key: &str, out: &mut String) {
    let (seventh, eighth) = match wait {
        true => (s(it.totals.unrecorded_ns), format!("{:>9}", it.woken_by_known)),
        // alloc: a cell
        false => (s(it.uncovered_ns), format!("{:>9.1}", s(it.unnamed_wait_ns))),
    };
    _ = writeln!(
        out,
        "{:>6} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {eighth}  {key}",
        it.count,
        s(it.wall_ns),
        s(it.totals.running_ns),
        s(it.totals.parked_ns),
        s(it.totals.queued_ns),
        s(it.totals.parked_or_queued_ns),
        seventh
    );
}

/// The path: its length against the wall time, its time by kind of link, and its `top`
/// longest links, in path order.
pub fn path(model: &Model, path: &Path, top: usize, out: &mut String) {
    let reached = match path.reached {
        // alloc: a label
        Reached::BuildStart => "the build's start".to_owned(),
        // alloc: a label
        Reached::LaneStart { lane } => {
            format!("the start of {} (the record names no spawner of it)", model.lanes[lane].id)
        }
        // alloc: a label
        Reached::Fuel { steps } => format!("its bound of {steps} steps: the path is cut short"),
        // alloc: a label
        Reached::NoRun => "nothing: no job ran".to_owned(),
    };
    _ = writeln!(
        out,
        "critical path: {} links, {:.1} s, {:.1}% of the wall time, back to {reached}",
        path.links.len(),
        s(path.len_ns()),
        pct(path.len_ns(), model.bounds.wall_ns())
    );
    for kind in LinkKind::ALL {
        let ns: i64 = path.links.iter().filter(|it| it.kind == kind).map(Link::dur_ns).sum();
        if ns > 0 {
            _ = writeln!(
                out,
                "  {:<10} {:>9.1} s {:>5.1}%",
                kind.name(),
                s(ns),
                pct(ns, path.len_ns())
            );
        }
    }
    // alloc: once a report
    let mut longest: Vec<usize> = (0..path.links.len()).collect();
    longest.sort_by_key(|&i| (std::cmp::Reverse(path.links[i].dur_ns()), i));
    longest.truncate(top);
    longest.sort_unstable();
    _ = writeln!(out, "  its {} longest links, in path order:", longest.len());
    _ = writeln!(
        out,
        "  {:>9} {:>9} {:<7} {:<12} {:>5}  {}",
        "at s", "dur s", "link", "lane", "core", "item"
    );
    for i in longest {
        let link = &path.links[i];
        let lane = &model.lanes[link.lane];
        let core = match lane.intervals[link.interval].state {
            // alloc: a cell
            State::Running { core: Some(core), .. } => core.to_string(),
            // alloc: a cell
            _ => "-".to_owned(),
        };
        let woke = link.wakes.map_or(String::new(), |wait| {
            // alloc: a cell
            format!(
                ", then woke {} ({})",
                model.lanes[model.waits[wait].lane].id,
                model.wait_label(&model.waits[wait])
            )
        });
        _ = writeln!(
            out,
            "  {:>9.1} {:>9.1} {:<7} {:<12} {:>5}  {}{woke}",
            s(link.from_ns - model.bounds.start_ns),
            s(link.dur_ns()),
            link.kind.name(),
            lane.id.to_string(),
            core,
            link_item(model, link)
        );
    }
}

/// The cores' busy time in `buckets` stretches of the build: a line each, how many cores were
/// busy on average and a bar of it.
pub fn cores(model: &Model, buckets: usize, out: &mut String) {
    let util = utilization(model);
    let width = (model.bounds.wall_ns() / buckets.max(1) as i64).max(1);
    let busy = core_busy(model, width);
    // alloc: once a report
    let mut by_bucket: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    for it in &busy {
        *by_bucket.entry(it.t0_ns).or_default() += it.running_ns;
    }
    _ = writeln!(
        out,
        "cores busy over the build ({} cores, {:.1} s a line):",
        util.cores,
        s(width)
    );
    for (t0, running) in by_bucket {
        let span = (width.min(model.bounds.end_ns - t0)).max(1);
        let busy = running as f64 / span as f64;
        let bar = (busy * 40.0 / f64::from(util.cores.max(1))).round() as usize;
        _ = writeln!(
            out,
            "  {:>8.1} s {:>5.1} {}",
            s(t0 - model.bounds.start_ns),
            busy,
            "#".repeat(bar)
        );
    }
}

/// The scheduling lines: the workers' idle time and how much of it sat beside queued work, the
/// critical path's running time by query, and the transcript simulator's replay of the build,
/// calibrated on the record's own workers, then with other cores and policies.
pub fn scheduling(model: &Model, path: &Path, top: usize, out: &mut String) {
    let idle = model.idle_with_work();
    _ = writeln!(
        out,
        "workers: {}, idle {:.1}% of workers x wall ({:.1} s); idle beside a job waiting for a core {:.2}% ({:.1} s){}",
        idle.workers,
        100.0 * idle.share(idle.idle_ns),
        s(idle.idle_ns),
        100.0 * idle.share(idle.with_work_ns),
        s(idle.with_work_ns),
        if idle.submits_said {
            ""
        } else {
            ", the record saying no submit's wait (no `queued_ns`): only waits after a `ready` row or a move"
        }
    );
    let on_path = super::path::by_query(model, path);
    let ran: i64 = on_path.iter().map(|it| it.ns).sum();
    _ = writeln!(
        out,
        "critical path by query: {:.1} s running, its {} longest:",
        s(ran),
        top.min(on_path.len())
    );
    for it in on_path.iter().take(top) {
        let label = match (&it.name, &it.subject) {
            // alloc: a cell
            (Some(name), Some(of)) => format!("{name} {of}"),
            // clone: a cell
            (Some(name), None) => name.clone(),
            // alloc: a cell
            (None, _) => "inside no span".to_owned(),
        };
        _ = writeln!(
            out,
            "  {:>9.1} s {:>5.1}% {:>7}x  {label}",
            s(it.ns),
            pct(it.ns, ran),
            it.stretches
        );
    }
    let transcript = super::sim::Transcript::of(model);
    _ = writeln!(
        out,
        "simulator: {} jobs ({} spawned, {} roots released at their time), {} runs costing {:.1} s, waits {} woken by a job, {} timed as recorded, {} moves",
        transcript.jobs.len(),
        transcript.spawned,
        transcript.roots,
        transcript.runs,
        s(transcript.cost_ns),
        transcript.woken,
        transcript.timed,
        transcript.moves
    );
    let calibration = transcript.calibrate();
    let p = calibration.predicted;
    _ = writeln!(
        out,
        "  calibrated on {} cores, {}, {:.1} us a run: wall {:.1} s (measured {:.1} s), idle {:.1}% (measured {:.1}%): {:.1} points off{}",
        p.cores,
        p.policy.name(),
        calibration.overhead_ns as f64 / 1e3,
        s(p.wall_ns),
        s(calibration.measured_wall_ns),
        100.0 * p.idle_share(),
        100.0 * calibration.measured_idle,
        calibration.error_points,
        match (p.stuck, p.out_of_fuel) {
            (0, false) => String::new(),
            // alloc: a line
            (stuck, fuel) => format!(
                ", {stuck} jobs stuck ({} on a wait, {} never started){}",
                p.stuck_waiting,
                stuck - p.stuck_waiting,
                if fuel { ", out of fuel" } else { "" }
            ),
        }
    );
    _ = writeln!(
        out,
        "  to the record's last run ({:.1} s; {:.1} s after it are no job's): idle {:.1}% predicted, {:.1}% measured: {:.1} points off",
        s(calibration.measured_last_run_ns),
        s(calibration.measured_wall_ns - calibration.measured_last_run_ns),
        100.0 * p.idle_share(),
        100.0 * calibration.measured_idle_to_last_run,
        calibration.error_points_to_last_run
    );
    let unbounded = transcript.replay(u32::MAX, super::sim::Policy::Fifo, calibration.overhead_ns);
    let workers = transcript.workers.max(1);
    let t1 = transcript.cost_ns + calibration.overhead_ns * transcript.runs as i64;
    let bound = (t1 / i64::from(workers)).max(unbounded.wall_ns);
    _ = writeln!(
        out,
        "  T1 {:.1} s, T1/P {:.1} s, T_inf {:.1} s (unbounded cores, at most {} busy): wall / max(T1/P, T_inf) = {:.2}",
        s(t1),
        s(t1 / i64::from(workers)),
        s(unbounded.wall_ns),
        unbounded.peak,
        model.bounds.wall_ns() as f64 / bound.max(1) as f64
    );
    for (cores, policy) in [
        (1, super::sim::Policy::Fifo),
        (workers.div_ceil(2), super::sim::Policy::Fifo),
        (workers, super::sim::Policy::Lifo),
        (workers.saturating_mul(2), super::sim::Policy::Fifo),
    ] {
        let it = transcript.replay(cores, policy, calibration.overhead_ns);
        _ = writeln!(
            out,
            "  {:>4} cores, {}: wall {:.1} s, idle {:.1}%",
            cores,
            policy.name(),
            s(it.wall_ns),
            100.0 * it.idle_share()
        );
    }
}
