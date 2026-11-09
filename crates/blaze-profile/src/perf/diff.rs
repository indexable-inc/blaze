//! Two runs compared function by function (docs/v2/plan/design/perf-history.md): joined by
//! identity, each change with its noise bound and its cause class, sorted by construction.
//!
//! The bound: per side, the variance of a function's samples across the side's repetitions,
//! never less than their mean (a sampled count has at least Poisson noise), so the bound of the
//! difference of the means is `Z * sqrt(var_run / n_run + var_base / n_base)`, in nanoseconds
//! through each side's sample period. A change inside it is "within noise": counted and summed
//! in the summary, never listed as faster or slower.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Write as _,
    path::Path,
};

use serde_derive::Serialize;

use super::{
    Id, hex,
    map::{self, Map},
    rows::{FnRow, RunRow},
};

/// How many standard deviations a change must pass to count as one.
pub const Z: f64 = 3.0;
/// The changed callees a row names at most.
pub const CALLEES: usize = 5;

/// One side of a diff: the run's row, as far as a reader needs it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Side {
    pub run: String,
    pub target: String,
    pub started_unix_ns: i64,
    pub reps: u32,
    pub tree: Option<String>,
    pub blaze: String,
    pub binary: Option<String>,
    pub host: String,
    pub cpu_period_ns: u64,
    pub gaps: Vec<String>,
}

/// A number on both sides: the mean per repetition of each, their difference and its bound.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Change {
    pub base: f64,
    pub run: f64,
    pub delta: f64,
    pub bound: f64,
    pub within_noise: bool,
}

/// Why a function's time may have changed, read from its identities on both sides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Cause {
    /// Its own code differs: its source, or the facts of its type arguments.
    CodeChanged,
    /// Same code, other codegen settings (toolchain, backend, opt level, target).
    SettingsChanged,
    /// Same code and settings; something it calls, inlines or reads changed.
    CalleeChanged,
    /// Nothing blaze built for it changed: layout, release inlining's whole-program inputs, or
    /// the machine.
    SameCodeAndCallees,
    /// Only in the run.
    Added,
    /// Only in the base.
    Removed,
    /// No identity on a side: the reason.
    Unknown { why: String },
}

impl Cause {
    pub fn text(&self) -> String {
        match self {
            // alloc: a row's text
            Cause::CodeChanged => "its code changed".to_owned(),
            // alloc: a row's text
            Cause::SettingsChanged => "built with other settings".to_owned(),
            // alloc: a row's text
            Cause::CalleeChanged => "same code, a callee changed".to_owned(),
            // alloc: a row's text
            Cause::SameCodeAndCallees => {
                "same code and callees: layout, inlining or environment".to_owned()
            }
            // alloc: a row's text
            Cause::Added => "added".to_owned(),
            // alloc: a row's text
            Cause::Removed => "removed".to_owned(),
            // alloc: a row's text
            Cause::Unknown { why } => format!("unknown: {why}"),
        }
    }
}

/// What a row's self time did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Slower,
    Faster,
    WithinNoise,
    Added,
    Removed,
}

/// One function.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub function: String,
    pub module: Option<String>,
    pub identity: Option<String>,
    pub cause: Cause,
    pub verdict: Verdict,
    /// Nanoseconds a repetition with it running.
    pub self_ns: Change,
    /// Nanoseconds a repetition with it on the stack.
    pub total_ns: Change,
    /// For [`Cause::CalleeChanged`]: the callees whose own code or settings changed, at most
    /// [`CALLEES`], sorted.
    pub changed_callees: Vec<String>,
    /// Why `changed_callees` is short or empty, when it is.
    pub callee_note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub slower: u32,
    pub faster: u32,
    pub within_noise: u32,
    pub added: u32,
    pub removed: u32,
    /// The within-noise rows' self changes, summed: what the noise adds up to.
    pub within_noise_net_ns: f64,
    /// On-CPU time a repetition, every function's self time.
    pub cpu_ns: Change,
}

/// A whole comparison: `blaze perf diff`'s JSON.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Diff {
    pub run: Side,
    pub base: Side,
    /// Reasons the two may not compare, and what a side lacks.
    pub notes: Vec<String>,
    pub summary: Summary,
    /// Beyond noise first, then by the size of the self change, then by function and identity.
    pub rows: Vec<Row>,
}

/// A function's join key: its identity, or for one no map names its module's file name and
/// symbol, so its time still lines up.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Join {
    Identity(String),
    Named { module: Option<String>, function: String },
}

/// A function on one side.
struct Function<'a> {
    row: &'a FnRow,
    self_samples: Vec<u64>,
    total_samples: Vec<u64>,
}

fn side(run: &RunRow) -> Result<Side, String> {
    let Some(period) = run.cpu_period_ns else {
        // alloc: an error
        return Err(format!(
            "run {} has no sample period: its sampler was off, so its samples weigh nothing; record a run with \
             the CPU sampler on",
            run.run
        ));
    };
    Ok(Side {
        // clone: once a diff
        run: run.run.clone(),
        // clone: once a diff
        target: run.target.clone(),
        started_unix_ns: run.started_unix_ns,
        reps: run.reps,
        // clone: once a diff
        tree: run.tree.clone(),
        // clone: once a diff
        blaze: run.blaze.clone(),
        // clone: once a diff
        binary: run.binary.clone(),
        // clone: once a diff
        host: run.host.clone(),
        cpu_period_ns: period,
        // alloc: once a diff
        gaps: run
            .gaps
            .as_deref()
            .map_or_else(Vec::new, |it| it.split("; ").map(str::to_owned).collect()),
    })
}

fn functions<'a>(run: &RunRow, rows: &'a [FnRow]) -> Result<BTreeMap<Join, Function<'a>>, String> {
    // alloc: once a side
    let mut out: BTreeMap<Join, Function<'a>> = BTreeMap::new();
    let reps = run.reps as usize;
    for row in rows {
        if row.run != run.run {
            // alloc: an error
            return Err(format!("a function row of run {} is given as run {}'s", row.run, run.run));
        }
        let rep = row.rep as usize;
        if rep >= reps {
            // alloc: an error
            return Err(format!(
                "run {} has {reps} repetitions, and {}'s row is of repetition {rep}",
                run.run, row.function
            ));
        }
        let join = match &row.identity {
            // clone: once a row
            Some(identity) => Join::Identity(identity.clone()),
            None => Join::Named {
                // alloc: once a row
                module: row
                    .module
                    .as_deref()
                    .map(|it| it.rsplit('/').next().unwrap_or(it).to_owned()),
                // clone: once a row
                function: row.function.clone(),
            },
        };
        let it = out.entry(join).or_insert_with(|| Function {
            row,
            // alloc: once a function
            self_samples: vec![0; reps],
            // alloc: once a function
            total_samples: vec![0; reps],
        });
        it.self_samples[rep] += row.self_samples;
        it.total_samples[rep] += row.total_samples;
    }
    Ok(out)
}

/// The mean of `counts` and the variance of one count, never under the mean (Poisson).
fn stats(counts: &[u64]) -> (f64, f64) {
    let n = counts.len() as f64;
    if counts.is_empty() {
        return (0.0, 0.0);
    }
    let mean = counts.iter().map(|it| *it as f64).sum::<f64>() / n;
    let spread = if counts.len() > 1 {
        counts.iter().map(|it| (*it as f64 - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    (mean, spread.max(mean))
}

/// The change between `base` and `run`, sample counts per repetition, in nanoseconds.
pub fn change(base: &[u64], base_period: u64, run: &[u64], run_period: u64) -> Change {
    let (base_mean, base_var) = stats(base);
    let (run_mean, run_var) = stats(run);
    let (pb, pr) = (base_period as f64, run_period as f64);
    let per = |var: f64, n: usize| if n == 0 { 0.0 } else { var / n as f64 };
    let bound =
        Z * (per(base_var, base.len()) * pb * pb + per(run_var, run.len()) * pr * pr).sqrt();
    let (base, run) = (base_mean * pb, run_mean * pr);
    let delta = run - base;
    Change { base, run, delta, bound, within_noise: delta.abs() <= bound }
}

/// Compares `run` (its row and function rows) against `base`, reading the code maps the run
/// rows name for the callees that changed.
pub fn compare(run: (&RunRow, &[FnRow]), base: (&RunRow, &[FnRow])) -> Result<Diff, String> {
    let (run_side, base_side) = (side(run.0)?, side(base.0)?);
    let (run_fns, base_fns) = (functions(run.0, run.1)?, functions(base.0, base.1)?);
    // alloc: once a diff
    let mut notes = Vec::new();
    if run.0.target != base.0.target {
        notes.push(format!("different programs: {} against {}", run.0.target, base.0.target));
    }
    if run.0.host != base.0.host {
        notes.push(format!("different hosts: {} against {}", run.0.host, base.0.host));
    }
    if (&run.0.arch, &run.0.os) != (&base.0.arch, &base.0.os) {
        notes.push(format!(
            "different machines: {}-{} against {}-{}",
            run.0.arch, run.0.os, base.0.arch, base.0.os
        ));
    }
    if run_side.cpu_period_ns != base_side.cpu_period_ns {
        notes.push(format!(
            "different sample periods: {} ns against {} ns",
            run_side.cpu_period_ns, base_side.cpu_period_ns
        ));
    }
    if run.0.blaze != base.0.blaze {
        // alloc: a note
        notes.push(
            "built by different blazes: while the toolchain's identity is blaze's own hash, every function's settings \
             differ"
                .to_owned(),
        );
    }
    for (name, it) in [("run", run.0), ("base", base.0)] {
        if it.reps < 2 {
            notes.push(format!(
                "one repetition in the {name}: bounds cover sampling noise only; repeat the run to bound run-to-run noise"
            ));
        }
    }
    for (name, it) in [("run", &run_side), ("base", &base_side)] {
        for gap in &it.gaps {
            notes.push(format!("{name} {}: {gap}", it.run));
        }
    }
    let mut graphs = Graphs { run: Lazy::new(run.0), base: Lazy::new(base.0) };

    let (pr, pb) = (run_side.cpu_period_ns, base_side.cpu_period_ns);
    let zeros = |n: u32| vec![0u64; n as usize];
    let (run_zeros, base_zeros) = (zeros(run.0.reps), zeros(base.0.reps));
    // alloc: once a diff
    let joins: BTreeSet<&Join> = run_fns.keys().chain(base_fns.keys()).collect();
    // alloc: once a diff, its rows
    let mut rows = Vec::with_capacity(joins.len());
    for join in joins {
        let (r, b) = (run_fns.get(join), base_fns.get(join));
        let pick = |it: Option<&Function<'_>>, total: bool, zeros: &[u64]| -> Vec<u64> {
            // clone: once a row, a side's counts
            it.map_or_else(
                || zeros.to_vec(),
                |it| if total { it.total_samples.clone() } else { it.self_samples.clone() },
            )
        };
        let self_ns = change(&pick(b, false, &base_zeros), pb, &pick(r, false, &run_zeros), pr);
        let total_ns = change(&pick(b, true, &base_zeros), pb, &pick(r, true, &run_zeros), pr);
        let shown = r.or(b).map(|it| it.row).ok_or("a joined function is on neither side")?;
        let cause = match (r.map(|it| it.row), b.map(|it| it.row)) {
            (Some(_), None) => Cause::Added,
            (None, Some(_)) => Cause::Removed,
            (Some(r), Some(b)) => cause(r, b),
            (None, None) => unreachable!("a join comes from a side"),
        };
        let verdict = match cause {
            Cause::Added => Verdict::Added,
            Cause::Removed => Verdict::Removed,
            _ if self_ns.within_noise => Verdict::WithinNoise,
            _ if self_ns.delta > 0.0 => Verdict::Slower,
            _ => Verdict::Faster,
        };
        // alloc: once a row
        let (mut changed_callees, mut callee_note) = (Vec::new(), None);
        if cause == Cause::CalleeChanged
            && verdict != Verdict::WithinNoise
            && let Some(identity) = &shown.identity
        {
            match graphs.changed_callees(identity) {
                Ok((names, note)) => {
                    changed_callees = names;
                    callee_note = note;
                }
                Err(why) => callee_note = Some(why),
            }
        }
        rows.push(Row {
            // clone: once a row
            function: shown.function.clone(),
            // clone: once a row
            module: shown.module.clone(),
            // clone: once a row
            identity: shown.identity.clone(),
            cause,
            verdict,
            self_ns,
            total_ns,
            changed_callees,
            callee_note,
        });
    }
    rows.sort_by(|a, b| {
        let beyond = |it: &Row| it.verdict == Verdict::WithinNoise;
        beyond(a)
            .cmp(&beyond(b))
            .then(b.self_ns.delta.abs().total_cmp(&a.self_ns.delta.abs()))
            .then_with(|| a.function.cmp(&b.function))
            .then_with(|| a.identity.cmp(&b.identity))
            .then_with(|| a.module.cmp(&b.module))
    });
    let count = |verdict: Verdict| rows.iter().filter(|it| it.verdict == verdict).count() as u32;
    let per_rep = |run: &RunRow, rows: &[FnRow]| {
        let mut out = vec![0u64; run.reps as usize];
        for row in rows {
            if let Some(it) = out.get_mut(row.rep as usize) {
                *it += row.self_samples;
            }
        }
        out
    };
    let summary = Summary {
        slower: count(Verdict::Slower),
        faster: count(Verdict::Faster),
        within_noise: count(Verdict::WithinNoise),
        added: count(Verdict::Added),
        removed: count(Verdict::Removed),
        within_noise_net_ns: rows
            .iter()
            .filter(|it| it.verdict == Verdict::WithinNoise)
            .map(|it| it.self_ns.delta)
            .sum(),
        cpu_ns: change(&per_rep(base.0, base.1), pb, &per_rep(run.0, run.1), pr),
    };
    Ok(Diff { run: run_side, base: base_side, notes, summary, rows })
}

/// The cause class of a function on both sides.
fn cause(run: &FnRow, base: &FnRow) -> Cause {
    let (Some(_), Some(_)) = (&run.identity, &base.identity) else {
        let why = run.gap.as_deref().or(base.gap.as_deref()).unwrap_or("no code map named it");
        // alloc: a row's cause
        return Cause::Unknown { why: why.to_owned() };
    };
    let missing = |it: &FnRow| it.code.is_none() || it.key.is_none() || it.deep.is_none();
    if missing(run) || missing(base) {
        // alloc: a row's cause
        return Cause::Unknown {
            why: "a side's row has an identity but no code, key or deep hash".to_owned(),
        };
    }
    if run.code != base.code {
        Cause::CodeChanged
    } else if run.key != base.key {
        Cause::SettingsChanged
    } else if run.deep != base.deep {
        Cause::CalleeChanged
    } else {
        Cause::SameCodeAndCallees
    }
}

/// A side's code map, read when a row first needs it.
enum Lazy<'a> {
    Unread(&'a RunRow),
    Read(Result<Graph, String>),
}

impl<'a> Lazy<'a> {
    fn new(run: &'a RunRow) -> Lazy<'a> {
        Lazy::Unread(run)
    }

    fn get(&mut self) -> &Result<Graph, String> {
        if let Lazy::Unread(run) = *self {
            *self = Lazy::Read(Graph::read(run));
        }
        match self {
            Lazy::Read(it) => it,
            Lazy::Unread(_) => unreachable!("just read"),
        }
    }
}

/// A code map with its `deep` hashes.
struct Graph {
    map: Map,
    deep: Vec<Id>,
}

impl Graph {
    fn read(run: &RunRow) -> Result<Graph, String> {
        let (Some(maps), Some(build)) = (&run.maps, &run.build_id) else {
            // alloc: an error
            return Err(format!("run {} names no code map of its executable", run.run));
        };
        let perf = Path::new(maps).parent().unwrap_or(Path::new(maps));
        match map::read(perf, build)? {
            Some((_, map)) => {
                let deep = map.deep()?;
                Ok(Graph { map, deep })
            }
            // alloc: an error
            None => Err(format!(
                "the code map of run {}'s build {build} is no longer kept (a link keeps the newest {} of an image), so \
                 the callees that changed can't be named",
                run.run,
                map::KEEP
            )),
        }
    }
}

struct Graphs<'a> {
    run: Lazy<'a>,
    base: Lazy<'a>,
}

impl Graphs<'_> {
    /// The callees of the function `identity` (hex) in the run whose own key is not in the
    /// base, reached through callees whose `deep` the base does not have: sorted, at most
    /// [`CALLEES`], with a note when there were more.
    fn changed_callees(&mut self, identity: &str) -> Result<(Vec<String>, Option<String>), String> {
        // alloc: an error's text, once a row
        let base = self.base.get().as_ref().map_err(String::clone)?;
        // alloc: once a row, the base's hashes
        let base_keys: BTreeSet<&Id> = base.map.nodes.iter().map(|it| &it.key).collect();
        // alloc: once a row, the base's hashes
        let base_deeps: BTreeSet<&Id> = base.deep.iter().collect();
        // alloc: an error's text, once a row
        let run = self.run.get().as_ref().map_err(String::clone)?;
        let nodes = &run.map.nodes;
        // alloc: once a row
        let mut queue: VecDeque<u32> = (0..nodes.len() as u32)
            .filter(|it| nodes[*it as usize].identity.is_some_and(|id| hex(&id) == identity))
            .collect();
        if queue.is_empty() {
            // alloc: a note
            return Ok((Vec::new(), Some(format!("{identity} is not in the run's code map"))));
        }
        // alloc: once a row
        let mut seen = vec![false; nodes.len()];
        for it in &queue {
            seen[*it as usize] = true;
        }
        // alloc: once a row
        let mut found: BTreeSet<&str> = BTreeSet::new();
        // Each node is queued once and each edge followed once.
        let mut fuel = nodes.len() + nodes.iter().map(|it| it.edges.len()).sum::<usize>() + 1;
        while let Some(at) = queue.pop_front() {
            for &callee in &nodes[at as usize].edges {
                fuel = fuel
                    .checked_sub(1)
                    .ok_or("the walk of changed callees ran past the code map's size")?;
                let c = callee as usize;
                if seen[c] || base_deeps.contains(&run.deep[c]) {
                    continue;
                }
                seen[c] = true;
                if base_keys.contains(&nodes[c].key) {
                    queue.push_back(callee);
                } else {
                    found.insert(&nodes[c].name);
                }
            }
        }
        let more = found.len().saturating_sub(CALLEES);
        // alloc: once a row
        let names: Vec<String> = found.into_iter().take(CALLEES).map(str::to_owned).collect();
        let note = match (names.is_empty(), more) {
            // alloc: a note
            (true, _) => Some(
                "no callee's own code or settings changed: what changed is outside the code map"
                    .to_owned(),
            ),
            (false, 0) => None,
            // alloc: a note
            (false, more) => Some(format!("and {more} more")),
        };
        Ok((names, note))
    }
}

/// `ns` nanoseconds, short.
pub fn duration(ns: f64) -> String {
    let abs = ns.abs();
    // alloc: a number's text
    if abs >= 1e9 {
        format!("{:.3} s", ns / 1e9)
    } else if abs >= 1e6 {
        format!("{:.2} ms", ns / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1} us", ns / 1e3)
    } else {
        format!("{ns:.0} ns")
    }
}

fn signed(ns: f64) -> String {
    // alloc: a number's text
    if ns > 0.0 { format!("+{}", duration(ns)) } else { duration(ns) }
}

fn verdict_text(it: Verdict) -> &'static str {
    match it {
        Verdict::Slower => "slower",
        Verdict::Faster => "faster",
        Verdict::WithinNoise => "within noise",
        Verdict::Added => "added",
        Verdict::Removed => "removed",
    }
}

/// The diff as text: header, notes, summary, then the rows beyond noise (every row with
/// `all`).
pub fn render(diff: &Diff, all: bool) -> String {
    // alloc: once a diff
    let mut out = String::new();
    let side = |out: &mut String, name: &str, it: &Side| {
        let _ = writeln!(
            out,
            "{name:<5} {}  {} rep{}  {}  host {}",
            it.run,
            it.reps,
            if it.reps == 1 { "" } else { "s" },
            it.target,
            it.host
        );
    };
    side(&mut out, "run", &diff.run);
    side(&mut out, "base", &diff.base);
    let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) if a == b => "same",
        (Some(_), Some(_)) => "differs",
        _ => "unknown",
    };
    let _ = writeln!(
        out,
        "tree {}, binary {}, blaze {}",
        same(&diff.run.tree, &diff.base.tree),
        same(&diff.run.binary, &diff.base.binary),
        if diff.run.blaze == diff.base.blaze { "same" } else { "differs" }
    );
    for note in &diff.notes {
        let _ = writeln!(out, "note: {note}");
    }
    let cpu = &diff.summary.cpu_ns;
    let _ = writeln!(
        out,
        "cpu a rep: {} -> {}  {} ± {}  {}",
        duration(cpu.base),
        duration(cpu.run),
        signed(cpu.delta),
        duration(cpu.bound),
        if cpu.within_noise {
            "within noise"
        } else if cpu.delta > 0.0 {
            "slower"
        } else {
            "faster"
        }
    );
    let s = &diff.summary;
    let _ = writeln!(
        out,
        "{} slower, {} faster, {} added, {} removed; {} within noise (net {})",
        s.slower,
        s.faster,
        s.added,
        s.removed,
        s.within_noise,
        signed(s.within_noise_net_ns)
    );
    let shown: Vec<&Row> =
        diff.rows.iter().filter(|it| all || it.verdict != Verdict::WithinNoise).collect();
    if shown.is_empty() {
        out.push_str("no function changed beyond noise\n");
        return out;
    }
    let _ = writeln!(
        out,
        "\n{:>11} {:>11}  {:>23}  {:<12}  function: cause",
        "self Δ", "± bound", "base -> run", ""
    );
    for row in shown {
        let _ = writeln!(
            out,
            "{:>11} {:>11}  {:>23}  {:<12}  {}: {}",
            signed(row.self_ns.delta),
            duration(row.self_ns.bound),
            format!("{} -> {}", duration(row.self_ns.base), duration(row.self_ns.run)),
            verdict_text(row.verdict),
            row.function,
            row.cause.text()
        );
        if !row.changed_callees.is_empty() {
            let _ = writeln!(out, "{:>63}callees changed: {}", "", row.changed_callees.join(", "));
        }
        if let Some(note) = &row.callee_note {
            let _ = writeln!(out, "{:>63}{note}", "");
        }
    }
    out
}

/// The run `base` defaults to: the latest run of `run`'s target before it, in `runs` (as
/// [`super::rows::runs`] orders them).
pub fn default_base<'a>(runs: &'a [RunRow], run: &RunRow) -> Option<&'a RunRow> {
    let at = runs.iter().position(|it| it.run == run.run)?;
    runs[..at].iter().rev().find(|it| it.target == run.target)
}

/// The run `name` names in `runs`: its whole name or a prefix of only it.
pub fn find<'a>(runs: &'a [RunRow], name: &str) -> Result<&'a RunRow, String> {
    // alloc: once a lookup
    let found: Vec<&RunRow> = runs.iter().filter(|it| it.run.starts_with(name)).collect();
    match found.as_slice() {
        [it] => Ok(it),
        // alloc: an error
        [] => Err(format!("no recorded run is named {name:?} (`blaze perf runs` lists them)")),
        // alloc: an error
        many => Err(format!(
            "{name:?} names {} runs ({}): give more of its name",
            many.len(),
            many.iter().map(|it| it.run.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}
