//! What made a build slow, and what to fix: `blaze slow` (docs/profiling.md, "What made a build
//! slow"). It reads one build's profile and gives findings ranked by the wall time each would
//! save if fixed, each with its evidence (numbers and the span, query or key it is about), the
//! kind of fix, how the saving was estimated and, when the span says, the module that owns it;
//! then what the profile could not see (time the record lost, waits with no name, CPU the spans
//! did not count), which is said, never skipped.
//!
//! Sans-IO: [`analyse`] is a pure function of the time model ([`crate::time::Model`]), its
//! critical path and the [`Facts`] (the tables that belong to no lane: the build's row, the
//! misses, the query engine's counts, the memory counters). [`load`] is the driver that reads
//! them through DuckDB; [`text`] renders a report for a person, and the report is
//! `serde::Serialize` for an agent (`blaze slow --json`). The output depends on nothing but the
//! rows: every map is sorted before it is read out (rule 25).
//!
//! What it borrows:
//! - EngFlow's bazel_invocation_analyzer: independent suggestion providers over one profile, each
//!   with a potential improvement; its bottleneck estimate (a stretch's time times the share of
//!   cores left idle) is [`Estimate::IdleCores`].
//! - Bazel's and Buck2's critical path, and Perfetto's thread-state wake-up chains: the path
//!   crosses from a wait to the job that woke it ([`crate::time::path`]), so a wait is charged
//!   only for the time it held the build up.
//! - Holistic Trace Analysis's idle-time breakdown: idle workers are split into "nothing was
//!   runnable" (the critical path's serial work) and "work was runnable but not run" (queued).

pub mod load;
mod render;
#[cfg(test)]
mod tests;

use std::alloc::Global;

use blaze_hash::{HashMap, HashSet};
use serde_derive::Serialize;

use crate::time::{
    Model, State, WAIT_SPAN,
    path::{Link, LinkKind, Path, Reached},
    report,
};

pub use render::{text, thread_line};

/// The version of the report's JSON: bumped when a field changes meaning or goes away. 2: an
/// unnamed wait's `hidden_ns` is the wall its waits cover, not their parked time summed, and
/// waits the record never ends are their own blind spot (`unended_wait`). 3: a build whose
/// daemon died is reported, to that daemon's last row, with `build.died`. 4: a build past
/// `time::load::SPAN_BUDGET` spans is modelled from its spans at or past a floor, said by the
/// blind spot `spans_cut`. 5: past the bound, the critical path's lanes are read again under
/// the floor and the job and wait spans under it are summed in `spans_cut`'s evidence (a floor
/// past which only unended spans are read is the bound, so no build is refused for its size); a
/// park whose wait span was under the floor is classed `<kind> wait under the read floor`, not an
/// unnamed wait; the workers' CPU counts every span, read or not.
/// 6: `build.panics`, the build's panics and its daemon's fatal signal, each with its stack's
/// top frames.
pub const VERSION: u32 = 6;

/// The build the report is of, as its `builds` row says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BuildFacts {
    pub id: u64,
    pub workspace: Option<String>,
    pub command: Option<String>,
    pub outcome: Option<String>,
    /// The digest of the tree it built, and the BLAKE3 of the blaze that built it (rule 22).
    pub tree: Option<String>,
    pub blaze: Option<String>,
    pub hits: Option<u64>,
    /// Nodes that ran their task, as the build counted them.
    pub misses: Option<u64>,
    /// How its daemon died during it: it has no end row, and is reported to that daemon's last.
    pub died: Option<Died>,
    /// The daemon process's user and system CPU over the build, from getrusage; `None` in the
    /// rows of a blaze before them, and while it runs.
    pub daemon_user_ns: Option<i64>,
    pub daemon_system_ns: Option<i64>,
    /// Its panics, and the fatal signal its daemon died of, oldest first (`panics` rows).
    pub panics: Vec<PanicFacts>,
}

/// A panic of a build, or the fatal signal its daemon died of: a `panics` row
/// ([`crate::schema::PANICS`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PanicFacts {
    /// `panic` or `signal`.
    pub kind: String,
    /// A job's boundary caught it: that job failed, the build went on.
    pub caught: bool,
    pub message: Option<String>,
    pub location: Option<String>,
    pub thread: Option<String>,
    pub signal: Option<String>,
    /// The frames a person reads first, innermost first, past the panic machinery's
    /// ([`crate::panics::top_frames`]).
    pub top: Vec<String>,
}

/// The death of a build's daemon ([`BuildFacts::died`]), and what it was running then.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Died {
    pub process: u32,
    /// Its last row (a span's end, a switch, a counter, an event), from the build's start: it
    /// died within one flush ([`crate::history::EVERY`]) of it.
    pub last_row_ns: i64,
    /// When the spans in `threads` were read, from the build's start; `None` when no reading
    /// was written in the build.
    pub read_ns: Option<i64>,
    /// The threads that were inside spans at that reading, in the order they were read.
    pub threads: Vec<DiedThread>,
}

/// A thread of a daemon that died, and the spans it was inside, outermost first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DiedThread {
    pub thread: u32,
    pub name: Option<String>,
    pub spans: Vec<DiedSpan>,
}

/// A span a thread was inside when its daemon was last read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DiedSpan {
    pub name: String,
    /// The query key or function it is of.
    pub function: Option<String>,
    /// What a wait waited for, and the job that ran it.
    pub awaits: Option<String>,
    pub owner: Option<u64>,
    /// How long it had run: by the daemon's last row in `blaze slow`, by the reading in
    /// [`crate::reading::Reading`].
    pub for_ns: i64,
}

/// What the report reads beside the time model.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub build: BuildFacts,
    /// The threads' names.
    pub threads: Vec<ThreadName>,
    /// The spans in the build's window by name and query kind.
    pub span_kinds: Vec<SpanKind>,
    /// The build's misses rows, by step and cause.
    pub misses: Vec<MissGroup>,
    pub query_counts: Vec<QueryCount>,
    /// The commonest sampled stacks inside the build's spans by span name, on and off the CPU,
    /// at most [`STACKS`] of each, sorted by name, then on before off, then most taken first.
    pub span_stacks: Vec<SpanStack>,
    /// Each thread's spans' CPU in the build's window, counted in DuckDB over every span.
    pub thread_cpu: Vec<ThreadCpu>,
    pub memory: Memory,
    pub recorder: Recorder,
    /// Which of the build's spans the model read ([`crate::time::load::SPAN_BUDGET`]).
    pub spans: crate::time::SpanCut,
}

/// A thread's spans' CPU over the build's window ([`Facts::thread_cpu`]): counted where every
/// span is, so a model that read only some of them ([`crate::time::SpanCut`]) judges its workers
/// by all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadCpu {
    pub process: u32,
    pub thread: u32,
    /// Its spans' `self_cpu_ns`, summed.
    pub cpu_ns: i64,
    /// Its spans with a `self_cpu_ns`, and those without.
    pub counted: u64,
    pub uncounted: u64,
}

/// What the flight recorder itself dropped or summed over the build's window, from its own
/// counters (`profile spans lost`, `profile rows skipped`, `profile rows skipped behind`, `profile
/// cost ns`, `profile spans made`, `profile cpu reads`) and the
/// `folded` table; `None` where the record has none (a blaze before them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Recorder {
    pub spans_lost: Option<i64>,
    /// Rows a thread's ring wrote over before the history read them, all told.
    pub rows_skipped: Option<i64>,
    /// Of `rows_skipped`, those skipped while the history's backlog was full (its writer behind).
    pub rows_behind: Option<i64>,
    /// Short spans summed into `folded` rows instead of a row each, and their time.
    pub folded_spans: Option<i64>,
    pub folded_ns: Option<i64>,
    /// The recorder's own measured cost over the window (`profile cost ns`): sampled hook time
    /// plus every thread-CPU read, and the spans (`profile spans made`) and CPU reads
    /// (`profile cpu reads`) it covers.
    pub cost_ns: Option<i64>,
    pub spans_made: Option<i64>,
    pub cpu_reads: Option<i64>,
}

impl Recorder {
    /// Rows lost, all told: spans the recorder could not open and rows rings wrote over.
    pub fn lost(&self) -> i64 {
        self.spans_lost.unwrap_or(0) + self.rows_skipped.unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadName {
    pub process: u32,
    pub thread: u32,
    pub name: String,
}

/// The spans of one name (and query kind) in the build's window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanKind {
    pub name: String,
    pub query_kind: Option<String>,
    /// The module its spans were made in (`spans.target`), the least of them.
    pub target: Option<String>,
    pub runs: u64,
    pub dur_ns: i64,
    pub self_cpu_ns: i64,
}

/// The misses rows of one step and cause.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MissGroup {
    pub step: String,
    pub cause: String,
    pub count: u64,
    /// The least item, for an example.
    pub example: String,
    /// The commonest `what`.
    pub what: Option<String>,
}

/// One query function's counts for the build (`query_counts`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryCount {
    pub query: String,
    pub executed: u64,
    pub verified: u64,
    pub reran: u64,
    pub evicted_asked: u64,
    pub evicted: u64,
    pub dropped: u64,
    /// Reads its runs recorded, a row read again in the same run included (`reads_recorded`).
    pub reads_recorded: u64,
    /// Reads its runs' rows kept: their distinct reads (`reads_kept`).
    pub reads_kept: u64,
    /// The most reads one run held on its query stack (`reads_most`, the build's most).
    pub reads_most: u64,
}

impl QueryCount {
    /// Reads one run may hold on its query stack before the report flags it: 2^20 `u32`s, 4 MiB
    /// of one thread's stack for one run, 64 times what a stored row's typical span holds.
    pub const MANY_READS: u64 = 1 << 20;
    /// Reads recorded over reads kept past which the report flags a query that re-reads: each
    /// row read 8 times in one run, on average.
    pub const REREAD_RATIO: u64 = 8;

    /// Its runs re-read the same rows many times, or one run held a huge stack of reads
    /// (`blaze slow`'s `reread`); a query that recorded fewer than [`Self::MANY_READS`] reads
    /// in the build is too small to matter.
    pub fn rereads(&self) -> bool {
        self.reads_most >= Self::MANY_READS
            || (self.reads_recorded >= Self::MANY_READS
                && self.reads_recorded >= Self::REREAD_RATIO.saturating_mul(self.reads_kept))
    }
}

/// The daemon's heap (`live bytes`) and footprint around the build, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// The last `live bytes` as the build before ended: what the daemon held after it.
    pub live_before: Option<i64>,
    /// The first `live bytes` in the build's window.
    pub live_first: Option<i64>,
    pub live_peak: Option<i64>,
    /// The least `live bytes` in the build's window.
    pub live_least: Option<i64>,
    pub footprint_peak: Option<i64>,
}

/// One report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub version: u32,
    pub build: BuildFacts,
    pub summary: Summary,
    /// Ranked: most wall time saved first; those whose saving can't be estimated last.
    pub findings: Vec<Finding>,
    /// What the profile could not see, most time hidden first.
    pub blind_spots: Vec<BlindSpot>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub wall_ns: i64,
    /// The pool's worker threads seen in the build.
    pub workers: u32,
    /// Their time running a job (or inside a span, for a thread no switch names), all told.
    pub busy_ns: i64,
    /// `busy_ns` over workers x wall.
    pub busy_ratio: f64,
    /// Every span's `self_cpu_ns` in the build's window, added.
    pub span_cpu_ns: i64,
    pub path_ns: i64,
    /// Where the critical path's walk stopped: the build's start, a lane's start, its bound.
    pub path_reached: String,
    /// The path's time by kind of link: ran, waited, wake, queued, spawned, unrecorded.
    pub path_by_link: Vec<Named>,
    /// The path's running time by the outermost span it ran in.
    pub path_by_phase: Vec<Named>,
    /// Findings left out for saving less than [`Summary::cut_ns`].
    pub smaller: u32,
    pub cut_ns: i64,
    /// What the flight recorder dropped and summed in the build's window.
    pub recorder: Recorder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Named {
    pub name: String,
    pub ns: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub kind: Kind,
    pub fix: Fix,
    /// The wall time it would save, by [`Finding::estimate`]; 0 with [`Estimate::Unknown`].
    pub saves_ns: i64,
    pub estimate: Estimate,
    /// What it is about: a span's name, a wait's class, a query.
    pub subject: String,
    /// The module or file that owns it, when the profile says.
    pub owner: Option<String>,
    pub title: String,
    pub evidence: Vec<String>,
    /// For a finding about a span name: the stacks the build's CPU samples inside its spans
    /// found most often, on and off the CPU, so it says where the time went inside them.
    #[serde(skip_serializing_if = "Stacks::is_empty")]
    pub stacks: Stacks,
}

/// The commonest sampled stacks inside a finding's spans ([`SpanStack`]), the most taken
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stacks {
    /// Of samples on a CPU (running or runnable): where it computed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub on_cpu: Vec<StackSeen>,
    /// Of samples off it (blocked inside the span): what it waited in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub off_cpu: Vec<StackSeen>,
}

impl Stacks {
    pub fn is_empty(&self) -> bool {
        self.on_cpu.is_empty() && self.off_cpu.is_empty()
    }
}

/// A stack and how many samples found it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StackSeen {
    /// Outermost first, each frame a function (inlined ones their own frames), never an offset.
    pub frames: Vec<String>,
    pub samples: u64,
}

/// One of the commonest stacks of the samples inside the spans of one name in the build, on or
/// off the CPU ([`Facts::span_stacks`]): what [`Stacks`] is made of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanStack {
    pub name: String,
    pub on_cpu: bool,
    pub frames: Vec<String>,
    pub samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Work on the critical path that ran serially while workers were idle.
    Serial,
    /// Work on the critical path that was on a core but got little CPU.
    Starved,
    /// Workers the OS gave much less CPU per second running than the others.
    StarvedWorkers,
    /// A kind of wait that held the build up.
    Wait,
    /// Runnable jobs that waited for a core.
    Queued,
    /// The same query key run more than once in the build.
    Redone,
    /// Runs in a view the row had already run in (`query_counts.reran`).
    Reran,
    /// Runs for a value evicted earlier and asked for again.
    Evicted,
    /// A query whose runs read the same rows many times, or held a huge stack of reads
    /// (`query_counts.reads_*`).
    Reread,
    /// The daemon let go of most of its heap between the build before and this one.
    Shed,
    /// Things the build computed again (the misses table), by cause.
    Missed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fix {
    Parallelise,
    GiveCpu,
    ShortenWait,
    WakeWorkers,
    KeyForReuse,
    KeepInMemory,
    KeepAcrossBuilds,
    ExplainMiss,
    ReadOnce,
}

impl Fix {
    /// The fix in words.
    pub fn says(self) -> &'static str {
        match self {
            Fix::Parallelise => {
                "parallelise it: it ran serially on the critical path while workers sat idle"
            }
            Fix::GiveCpu => {
                "give it CPU: it was on a core, or blocked outside any `wait` span, yet got little CPU; check its worker's QoS class, the host's load, and blocking calls no `wait` span names"
            }
            Fix::ShortenWait => {
                "shorten what it waits on, or start that sooner: the owner's work is what held the build up"
            }
            Fix::WakeWorkers => {
                "run runnable work at once: jobs sat queued for a core while workers were idle"
            }
            Fix::KeyForReuse => {
                "key it so its first run is reused: the same key ran more than once in one build"
            }
            Fix::KeepInMemory => {
                "keep it in memory: values were evicted, then asked for and computed again"
            }
            Fix::KeepAcrossBuilds => {
                "keep the lowered database across requests: the daemon let it go between builds"
            }
            Fix::ExplainMiss => {
                "find why each was computed again (`blaze why`) and key it so it is reused"
            }
            Fix::ReadOnce => {
                "read each row once a run: hoist reads out of loops over items, or read one query that gathers them; each read costs the run a query stack entry and its verify a check"
            }
        }
    }
}

/// How a finding's saving was worked out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Estimate {
    /// Its time on the critical path (times the share it could lose: idle workers, missing CPU).
    CriticalPath,
    /// Its wall time times the share of workers idle meanwhile: an upper bound, used where the
    /// critical path can't be trusted.
    IdleCores,
    /// Its thread time over the build's mean parallelism.
    ThreadTime,
    /// The profile does not say what it cost.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlindSpot {
    pub kind: BlindKind,
    /// The time it hides; 0 when it hides a count, not time.
    pub hidden_ns: i64,
    pub subject: String,
    pub title: String,
    pub evidence: Vec<String>,
    pub fix: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlindKind {
    /// Critical path time the record lost.
    Unrecorded,
    /// Workers running a job inside no span.
    Uncovered,
    /// Waits that say nothing of what they wait for (rule 15).
    UnnamedWait,
    /// Waits that name what they wait for but never who runs it.
    OwnerlessWait,
    /// Waits the record never ends: still waiting as it ends, or their resume row lost. Their
    /// parked time runs to the record's end, so they are kept out of the wait classes.
    UnendedWait,
    /// Workers' running time the spans counted no CPU for.
    CpuGap,
    /// No `ready` rows: parked and queued time can't be told apart.
    NoReadyRows,
    /// Misses the build counted that no misses row explains.
    MissesUnexplained,
    /// Rows that did not line up (spans with no parent row, runs cut short).
    Misaligned,
    /// The critical path's walk stopped short of the build's start.
    PathCut,
    /// Rows the flight recorder lost: spans it could not open, rows a ring wrote over.
    RowsLost,
    /// Spans under the floor the model read from, past its bound ([`crate::time::SpanCut`]).
    SpansCut,
}

/// The least saving a finding needs to be listed: 1% of the wall time, and at least this.
const MIN_SAVES_NS: i64 = 200_000_000;
/// A span whose CPU over its own running time is below this was starved.
const STARVED_RATIO: f64 = 0.25;
/// A worker whose CPU per running second is below the others' by this factor is starved.
const STARVED_WORKER_FACTOR: f64 = 3.0;
/// The least running time a span group or worker needs before its CPU rate is judged.
const JUDGED_NS: i64 = 1_000_000_000;
/// The least time a blind spot needs to be listed.
const MIN_HIDDEN_NS: i64 = 100_000_000;
/// The most examples a finding lists.
const EXAMPLES: usize = 3;

/// The report of `model`, its critical `path` and `facts`.
pub fn analyse(model: &Model, path: &Path, facts: &Facts) -> Report {
    let ctx = Ctx::new(model, path, facts);
    // alloc: a report's findings, once a report
    let mut findings = Vec::new();
    ctx.path_work(&mut findings);
    ctx.starved_workers(&mut findings);
    let mut blind = ctx.waits(&mut findings);
    ctx.queued(&mut findings);
    ctx.redone(&mut findings);
    ctx.query_counts(&mut findings);
    ctx.missed(&mut findings);
    ctx.shed(&mut findings);
    ctx.blind_spots(&mut blind);
    attach_stacks(&mut findings, &facts.span_stacks);

    let cut_ns = (model.bounds.wall_ns() / 100).max(MIN_SAVES_NS);
    let before = findings.len();
    findings.retain(|it| it.estimate == Estimate::Unknown || it.saves_ns >= cut_ns);
    let smaller = (before - findings.len()) as u32;
    findings.sort_by(|a, b| {
        (a.estimate == Estimate::Unknown)
            .cmp(&(b.estimate == Estimate::Unknown))
            .then(b.saves_ns.cmp(&a.saves_ns))
            .then(a.kind.cmp(&b.kind))
            .then_with(|| a.subject.cmp(&b.subject))
    });
    blind.retain(|it| it.hidden_ns >= MIN_HIDDEN_NS || it.hidden_ns == 0);
    blind.sort_by(|a, b| {
        b.hidden_ns
            .cmp(&a.hidden_ns)
            .then(a.kind.cmp(&b.kind))
            .then_with(|| a.subject.cmp(&b.subject))
    });
    Report {
        version: VERSION,
        build: facts.build.clone(),
        summary: ctx.summary(smaller, cut_ns),
        findings,
        blind_spots: blind,
    }
}

/// The most stacks on and off the CPU a finding lists.
pub const STACKS: usize = 3;

/// Gives each finding about a span name (its subject) the commonest stacks sampled inside
/// those spans. Findings about anything else (a query, a kind of wait, the workers) get none.
fn attach_stacks(findings: &mut [Finding], stacks: &[SpanStack]) {
    if stacks.is_empty() {
        return;
    }
    for finding in findings {
        let at = stacks.partition_point(|it| it.name.as_str() < finding.subject.as_str());
        for it in stacks[at..].iter().take_while(|it| it.name == finding.subject) {
            let side =
                if it.on_cpu { &mut finding.stacks.on_cpu } else { &mut finding.stacks.off_cpu };
            if side.len() < STACKS {
                // clone: a stack, once a finding it is attached to
                side.push(StackSeen { frames: it.frames.clone(), samples: it.samples });
            }
        }
    }
}

/// Integer seconds to a tenth, for evidence.
fn s(ns: i64) -> String {
    // alloc: a cell
    format!("{:.1} s", ns as f64 / 1e9)
}

fn pct(part: i64, whole: i64) -> f64 {
    if whole > 0 { 100.0 * part as f64 / whole as f64 } else { 0.0 }
}

/// What the analysis precomputes once.
struct Ctx<'a> {
    model: &'a Model,
    path: &'a Path,
    facts: &'a Facts,
    /// The pool's worker threads, sorted.
    workers: Vec<(u32, u32)>,
    /// Each worker's running stretches, merged and clipped to the build: `workers`' order.
    running: Vec<Vec<(i64, i64)>>,
    busy: Steps,
    /// The path's running time cut by the innermost span open: (from, to, span or none, lane).
    path_ran: Vec<(i64, i64, Option<usize>, usize)>,
    /// The critical path is complete enough to charge waits by it.
    path_trusted: bool,
}

impl<'a> Ctx<'a> {
    fn new(model: &'a Model, path: &'a Path, facts: &'a Facts) -> Ctx<'a> {
        let (start, end) = (model.bounds.start_ns, model.bounds.end_ns);
        let named: HashSet<(u32, u32), Global> = {
            let mut it = HashSet::new_in(Global);
            it.extend(
                facts
                    .threads
                    .iter()
                    .filter(|it| it.name.starts_with("blaze-work"))
                    .map(|it| (it.process, it.thread)),
            );
            it
        };
        // A worker: a thread a pool job ran on, or one named as the pool names its threads.
        let mut stretches: HashMap<(u32, u32), Vec<(i64, i64)>, Global> = HashMap::new_in(Global);
        for lane in &model.lanes {
            for it in &lane.intervals {
                let State::Running { thread, core, .. } = it.state else { continue };
                let key = (lane.id.process(), thread);
                if core.is_none() && !named.contains(&key) && !named.is_empty() {
                    continue;
                }
                let (from, to) = (it.from_ns.max(start), it.to_ns.min(end));
                if to > from {
                    // alloc: a worker's stretches, once a worker a report
                    stretches.entry(key).or_default().push((from, to));
                }
            }
        }
        for &key in &named {
            stretches.entry(key).or_default();
        }
        // alloc: once a report
        let mut workers: Vec<((u32, u32), Vec<(i64, i64)>)> = stretches.into_iter().collect();
        workers.sort_unstable_by_key(|it| it.0);
        for (_, it) in &mut workers {
            merge(it);
        }
        let busy = Steps::of(workers.iter().map(|it| it.1.as_slice()));
        let (workers, running): (Vec<_>, Vec<_>) = workers.into_iter().unzip();

        let path_ran = crate::time::path::segments(model, path);
        let lost: i64 =
            path.links.iter().filter(|it| it.kind == LinkKind::Unrecorded).map(Link::dur_ns).sum();
        let path_trusted =
            matches!(path.reached, Reached::BuildStart) && lost * 5 < path.len_ns().max(1);
        Ctx { model, path, facts, workers, running, busy, path_ran, path_trusted }
    }

    fn wall(&self) -> i64 {
        self.model.bounds.wall_ns()
    }

    fn n_workers(&self) -> i64 {
        (self.workers.len() as i64).max(1)
    }

    /// The build's mean parallelism: busy workers on average, at least 1.
    fn parallelism(&self) -> f64 {
        let (start, end) = (self.model.bounds.start_ns, self.model.bounds.end_ns);
        (self.busy.integral(start, end) as f64 / self.wall().max(1) as f64).max(1.0)
    }

    fn thread_name(&self, process: u32, thread: u32) -> String {
        match self.facts.threads.iter().find(|it| it.process == process && it.thread == thread) {
            // clone: a label
            Some(it) => it.name.clone(),
            // alloc: a label
            None => format!("thread {thread}"),
        }
    }

    /// The module that made spans named `name`, or a file named in `subject`.
    fn owner(&self, name: &str, subject: &str) -> Option<String> {
        if let Some(file) = file_in(subject) {
            return Some(file);
        }
        self.facts
            .span_kinds
            .iter()
            .filter(|it| it.name == name)
            .filter_map(|it| it.target.as_deref())
            .filter(|it| *it != "blaze")
            .min()
            .map(str::to_owned)
    }

    /// Serial and starved work on the critical path, by the innermost span it ran in.
    fn path_work(&self, out: &mut Vec<Finding>) {
        let model = self.model;
        #[derive(Default)]
        struct Group {
            ns: i64,
            busy_ns: i128,
            subjects: Vec<(String, i64)>,
            spans: Vec<usize>,
            roots: Vec<(String, i64)>,
        }
        let mut groups: HashMap<&str, Group, Global> = HashMap::new_in(Global);
        for &(from, to, span, _) in &self.path_ran {
            let ns = to - from;
            let name = span.map_or("inside no span", |it| model.spans[it].name.as_str());
            let group = groups.entry(name).or_default();
            group.ns += ns;
            group.busy_ns += i128::from(self.busy.integral(from, to));
            if let Some(span) = span {
                // alloc: an example, once a span a report
                let subject = subject_of(model, span).unwrap_or("").to_owned();
                group.subjects.push((subject, ns));
                group.spans.push(span);
                let root = model.root_of(span);
                if root != span {
                    // clone: a label, once a span a report
                    group.roots.push((model.spans[root].name.clone(), ns));
                }
            }
        }
        let workers = self.n_workers();
        for (name, mut group) in groups {
            group.spans.sort_unstable();
            group.spans.dedup();
            let own_running: i64 =
                group.spans.iter().map(|&i| model.span_time[i].own.running_ns).sum();
            let own_cpu: i64 =
                group.spans.iter().map(|&i| model.spans[i].self_cpu_ns.unwrap_or(0).max(0)).sum();
            let counted = group.spans.iter().any(|&i| model.spans[i].self_cpu_ns.is_some());
            let busy = (group.busy_ns as f64 / group.ns.max(1) as f64).max(1.0);
            let ratio = if own_running > 0 { own_cpu as f64 / own_running as f64 } else { 1.0 };
            let subjects = top(&mut group.subjects);
            let roots = top(&mut group.roots);
            // alloc: a finding's evidence
            let mut evidence = Vec::new();
            let count = group.spans.len();
            if subjects.iter().any(|it| !it.0.is_empty()) {
                evidence.push(format!("of {count} spans on the path: {}", list(&subjects)));
            }
            if !roots.is_empty() {
                evidence.push(format!("inside {}", list(&roots)));
            }
            evidence.push(format!(
                "{:.1} of {workers} workers busy on average meanwhile ({:.0}% idle)",
                busy,
                100.0 * (1.0 - busy / workers as f64).max(0.0)
            ));
            if counted {
                evidence.push(format!(
                    "its spans ran {} on a core and counted {} of CPU ({:.0}%)",
                    s(own_running),
                    s(own_cpu),
                    100.0 * ratio
                ));
            }
            let owner = self.owner(name, subjects.first().map_or("", |it| it.0.as_str()));
            // alloc: a finding's subject
            let subject = name.to_owned();
            if counted && own_running >= JUDGED_NS && ratio < STARVED_RATIO {
                out.push(Finding {
                    stacks: Stacks::default(),
                    kind: Kind::Starved,
                    fix: Fix::GiveCpu,
                    saves_ns: (group.ns as f64 * (1.0 - ratio)) as i64,
                    estimate: Estimate::CriticalPath,
                    title: format!(
                        "starved: `{name}` held the critical path {} and its spans got {:.0}% of a CPU",
                        s(group.ns),
                        100.0 * ratio
                    ),
                    subject,
                    owner,
                    evidence,
                });
            } else if busy < 0.5 * workers as f64 {
                out.push(Finding {
                    stacks: Stacks::default(),
                    kind: Kind::Serial,
                    fix: Fix::Parallelise,
                    saves_ns: (group.ns as f64 * (1.0 - busy / workers as f64)) as i64,
                    estimate: Estimate::CriticalPath,
                    title: format!(
                        "serial: `{name}` ran {} on the critical path while most workers were idle",
                        s(group.ns)
                    ),
                    subject,
                    owner,
                    evidence,
                });
            }
        }
    }

    /// Workers the OS gave much less CPU per running second than the others.
    fn starved_workers(&self, out: &mut Vec<Finding>) {
        let mut cpu: HashMap<(u32, u32), i64, Global> = HashMap::new_in(Global);
        let mut counted = false;
        for it in self.facts.thread_cpu.iter().filter(|it| it.counted > 0) {
            counted = true;
            *cpu.entry((it.process, it.thread)).or_default() += it.cpu_ns.max(0);
        }
        if !counted {
            return;
        }
        // alloc: once a report
        let mut rates: Vec<(usize, i64, i64, f64)> = Vec::new();
        for (w, key) in self.workers.iter().enumerate() {
            let running: i64 = self.running[w].iter().map(|it| it.1 - it.0).sum();
            if running < JUDGED_NS {
                continue;
            }
            let used = cpu.get(key).copied().unwrap_or(0);
            rates.push((w, running, used, used as f64 / running as f64));
        }
        if rates.len() < 2 {
            return;
        }
        // alloc: once a report
        let mut sorted: Vec<f64> = rates.iter().map(|it| it.3).collect();
        sorted.sort_by(|a, b| b.total_cmp(a));
        let upper = &sorted[..sorted.len().div_ceil(2)];
        let reference = upper[upper.len() / 2];
        if reference <= 0.0 {
            return;
        }
        let starved: Vec<&(usize, i64, i64, f64)> =
            rates.iter().filter(|it| it.3 * STARVED_WORKER_FACTOR < reference).collect();
        if starved.is_empty() {
            return;
        }
        // The path's running time on each starved worker.
        let mut saves = 0.0;
        for &(from, to, _, lane) in &self.path_ran {
            let Some(interval) = self.model.lanes[lane].at_or_before(to) else { continue };
            let State::Running { thread, .. } = self.model.lanes[lane].intervals[interval].state
            else {
                continue;
            };
            let key = (self.model.lanes[lane].id.process(), thread);
            if let Some(it) = starved.iter().find(|it| self.workers[it.0] == key) {
                saves += (to - from) as f64 * (1.0 - it.3 / reference);
            }
        }
        let carried: i64 = starved.iter().map(|it| it.1).sum();
        let all: i64 = rates.iter().map(|it| it.1).sum();
        let names: Vec<String> = starved
            .iter()
            .take(6)
            .map(|it| {
                let (process, thread) = self.workers[it.0];
                format!("{} {:.3}", self.thread_name(process, thread), it.3)
            })
            .collect();
        let more = starved.len().saturating_sub(6);
        let mut evidence = vec![
            format!(
                "{} of {} workers got under 1/{STARVED_WORKER_FACTOR:.0} of the CPU per running second the others got ({reference:.3}): {}{}",
                starved.len(),
                rates.len(),
                names.join(", "),
                if more > 0 { format!(" and {more} more") } else { String::new() }
            ),
            format!(
                "they carried {:.0}% of the workers' running time ({} of {})",
                pct(carried, all),
                s(carried),
                s(all)
            ),
        ];
        if saves < 1.0 {
            evidence.push("none of the critical path ran on them".to_owned());
        }
        out.push(Finding {
            stacks: Stacks::default(),
            kind: Kind::StarvedWorkers,
            fix: Fix::GiveCpu,
            saves_ns: saves as i64,
            estimate: Estimate::CriticalPath,
            subject: "workers".to_owned(),
            owner: Some("blaze_graph::pool".to_owned()),
            title: format!(
                "starved workers: {} workers ran at a fraction of the others' CPU rate",
                starved.len()
            ),
            evidence,
        });
    }

    /// Waits by what they wait for, charged by the critical path; and the blind spots of waits
    /// that name nothing or no owner.
    fn waits(&self, out: &mut Vec<Finding>) -> Vec<BlindSpot> {
        let model = self.model;
        #[derive(Default)]
        struct Class {
            count: u64,
            ns: i64,
            on_path: Vec<(i64, i64)>,
            stretches: Vec<(i64, i64)>,
            owners: Vec<(String, i64)>,
            /// The packages a class over many (`scope of unit codegen (build script)`) was of.
            of: Vec<(String, i64)>,
            known: u64,
            named: bool,
            /// Its waits' spans were under the floor the model read from ([`crate::time::SpanCut`]).
            under_floor: bool,
            moved: bool,
        }
        // alloc: once a report
        let mut of_wait = vec![0i64; model.waits.len()];
        // alloc: once a report
        let mut stretches: Vec<Vec<(i64, i64)>> = vec![Vec::new(); model.waits.len()];
        for lane in &model.lanes {
            for it in &lane.intervals {
                if let Some(w) = it.state.wait() {
                    of_wait[w] += it.dur_ns();
                    stretches[w].push((it.from_ns, it.to_ns));
                }
            }
        }
        // The path's time while each wait held it up: a wait it crossed to its waker stood on
        // the path the whole time it waited (the waker's run stands in its place); a wait it
        // could not cross, and a wake's latency, are links of their own.
        // alloc: once a report
        let mut path_of_wait: Vec<Vec<(i64, i64)>> = vec![Vec::new(); model.waits.len()];
        let (path_from, path_to) =
            (self.path.start_ns().unwrap_or(0), self.path.end_ns().unwrap_or(0));
        for link in &self.path.links {
            if let Some(w) = link.wakes {
                let clipped = stretches[w]
                    .iter()
                    .map(|&(from, to)| (from.max(path_from), to.min(path_to)))
                    .filter(|it| it.1 > it.0);
                path_of_wait[w].extend(clipped);
            }
            if matches!(link.kind, LinkKind::Waited | LinkKind::Wake)
                && let Some(w) = model.lanes[link.lane].intervals[link.interval].state.wait()
            {
                path_of_wait[w].push((link.from_ns, link.to_ns));
            }
        }
        let mut classes: HashMap<String, Class, Global> = HashMap::new_in(Global);
        // Waits the record never ends: their time runs to its end, summed over every one
        // (65,439 of them made millions of seconds of one build), so they are told apart.
        let mut unended = Class::default();
        for (w, wait) in model.waits.iter().enumerate() {
            if of_wait[w] == 0 {
                continue;
            }
            // A wait with no `awaits` is classed by its kind alone: its value says nothing a
            // reader can act on, and one row per value would bury the rest.
            let named = wait.span.and_then(|it| model.spans[it].awaits.as_deref()).is_some();
            // A wait span holds its park, so a park as long as the floor had its span read: one
            // shorter, with none, may have had one the model did not read.
            let floor = self.facts.spans.floor_ns;
            let under_floor = !named
                && floor > 0
                && wait.resumed_ns.is_some_and(|end| end - wait.parked_ns < floor);
            let kind = wait.on.map_or("span", |it| it.name());
            let (class, of) = match (named, under_floor) {
                (true, _) => by_package(report::wait_class(model, wait)),
                // alloc: a row's key
                (false, true) => (format!("{kind} wait under the read floor"), None),
                // alloc: a row's key
                (false, false) => (format!("{kind} wait with no wait span"), None),
            };
            if wait.resumed_ns.is_none() {
                unended.count += 1;
                unended.ns += of_wait[w];
                unended.stretches.append(&mut stretches[w]);
                unended.of.push((class, of_wait[w]));
                continue;
            }
            let class = classes.entry(class).or_default();
            if let Some(of) = of {
                class.of.push((of, of_wait[w]));
            }
            class.count += 1;
            class.ns += of_wait[w];
            class.on_path.append(&mut path_of_wait[w]);
            class.stretches.append(&mut stretches[w]);
            class.named |= named;
            class.under_floor |= under_floor;
            class.moved |= wait.on == Some(crate::flight::WaitedOn::Moved);
            if let Some(lane) = wait.waker.lane() {
                class.known += 1;
                let at = wait.ready_ns.or(wait.resumed_ns).unwrap_or(wait.parked_ns);
                class.owners.push((doing_at(model, lane, at), of_wait[w]));
            }
        }
        // alloc: once a report
        let mut classes: Vec<(String, Class)> = classes.into_iter().collect();
        classes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let workers = self.n_workers();
        // alloc: once a report
        let mut blind = Vec::new();
        for (name, mut class) in classes {
            if class.moved {
                continue;
            }
            merge(&mut class.stretches);
            merge(&mut class.on_path);
            let on_path: i64 = class.on_path.iter().map(|it| it.1 - it.0).sum();
            let wall: i64 = class.stretches.iter().map(|it| it.1 - it.0).sum();
            let busy: i64 = class.stretches.iter().map(|it| self.busy.integral(it.0, it.1)).sum();
            let idle = (1.0 - busy as f64 / (workers as f64 * wall.max(1) as f64)).clamp(0.0, 1.0);
            let owners = top(&mut class.owners);
            let mut evidence = vec![format!(
                "{} waits, {} parked in all (summed over the jobs waiting, not workers' time), {} of wall with one or more waiting, {:.0}% of workers idle meanwhile",
                class.count,
                s(class.ns),
                s(wall),
                100.0 * idle
            )];
            if on_path > 0 {
                evidence.push(format!("{} of it on the critical path", s(on_path)));
            }
            if !class.of.is_empty() {
                evidence.push(format!("of: {}", list(&top(&mut class.of))));
            }
            match owners.is_empty() {
                false => evidence.push(format!("woken by: {}", list(&owners))),
                true => evidence.push("no wait of it names who woke it".to_owned()),
            }
            if class.under_floor {
                evidence.push(format!(
                    "their wait spans, if they opened any, were under the {} floor the model read from: what such waits were on is summed under `spans_cut`",
                    s(self.facts.spans.floor_ns)
                ));
            }
            if !class.named && !class.under_floor {
                match self.facts.recorder.spans_lost {
                    Some(0) => evidence.push("the recorder lost no span in the build's window: the wait opened none".to_owned()),
                    Some(lost) => evidence.push(format!(
                        "the recorder lost {lost} spans in the build's window (`profile spans lost`): a wait's span may be one of them"
                    )),
                    None => evidence.push("the record has no `profile spans lost` counter: a lost span can't be told from a wait that opened none".to_owned()),
                }
                blind.push(BlindSpot {
                    kind: BlindKind::UnnamedWait,
                    hidden_ns: wall,
                    title: format!("waits on `{name}` say nothing of what they wait for: {} of wall, {} parked in all", s(wall), s(class.ns)),
                    subject: name,
                    evidence,
                    fix: "open a `wait` span naming the awaited key, lock, channel or child, and its owner (rule 15); or, when the recorder lost spans, find what held its open slots".to_owned(),
                });
                continue;
            }
            if class.named && class.known == 0 && class.ns >= JUDGED_NS {
                blind.push(BlindSpot {
                    kind: BlindKind::OwnerlessWait,
                    hidden_ns: class.ns,
                    title: format!("waits on `{name}` never name who runs what they wait for: {} parked in all", s(class.ns)),
                    // clone: a blind spot's subject
                    subject: name.clone(),
                    // clone: its evidence, also the finding's
                    evidence: evidence.clone(),
                    fix: "set the wait's `owner` (the job running the key, the lock's holder) so the critical path can cross it".to_owned(),
                });
            }
            let (saves, estimate) = match (on_path > 0, self.path_trusted) {
                (true, _) => (on_path, Estimate::CriticalPath),
                (false, true) => (0, Estimate::CriticalPath),
                (false, false) => ((wall as f64 * idle) as i64, Estimate::IdleCores),
            };
            let owner = self
                .owner(WAIT_SPAN, &name)
                .or_else(|| owners.first().and_then(|it| file_in(&it.0)));
            out.push(Finding {
                stacks: Stacks::default(),
                kind: Kind::Wait,
                fix: Fix::ShortenWait,
                saves_ns: saves,
                estimate,
                title: format!(
                    "waits on `{name}`: {} held up",
                    s(if on_path > 0 { on_path } else { wall })
                ),
                subject: name,
                owner,
                evidence,
            });
        }
        if unended.count > 0 {
            merge(&mut unended.stretches);
            let wall: i64 = unended.stretches.iter().map(|it| it.1 - it.0).sum();
            let mut evidence = vec![
                format!("{} parked in all, each to the record's end; by class: {}", s(unended.ns), list(&top(&mut unended.of))),
                "still waiting as the record ends (a stalled build) or their resumed row lost (`profile rows skipped`)".to_owned(),
            ];
            if let Some(skipped) = self.facts.recorder.rows_skipped.filter(|it| *it > 0) {
                evidence.push(format!("the recorder skipped {skipped} rows in the build's window"));
            }
            blind.push(BlindSpot {
                kind: BlindKind::UnendedWait,
                hidden_ns: wall,
                subject: "waits".to_owned(),
                title: format!("{} waits never end in the record: {} of wall with one open, left out of the waits above", unended.count, s(wall)),
                evidence,
                fix: "a stalled build: its open waits and their owners (`blaze debug stalls`); rows skipped: drain the rings sooner".to_owned(),
            });
        }
        blind
    }

    /// Runnable jobs that waited for a core on the critical path.
    fn queued(&self, out: &mut Vec<Finding>) {
        let on_path: i64 = self
            .path
            .links
            .iter()
            .filter(|it| matches!(it.kind, LinkKind::Queued | LinkKind::Spawned))
            .map(Link::dur_ns)
            .sum();
        let all = report::job_totals(self.model).queued_ns;
        if on_path == 0 {
            return;
        }
        out.push(Finding {
            stacks: Stacks::default(),
            kind: Kind::Queued,
            fix: Fix::WakeWorkers,
            saves_ns: on_path,
            estimate: Estimate::CriticalPath,
            subject: "queued for a core".to_owned(),
            owner: Some("blaze_graph::pool".to_owned()),
            title: format!(
                "queued: the critical path waited {} for a core while its work was runnable",
                s(on_path)
            ),
            evidence: vec![format!("{} of queued time over all jobs", s(all))],
        });
    }

    /// The same query key run more than once in the build.
    fn redone(&self, out: &mut Vec<Finding>) {
        let model = self.model;
        let queries: HashSet<&str, Global> = {
            let mut it = HashSet::new_in(Global);
            it.extend(
                self.facts
                    .span_kinds
                    .iter()
                    .filter(|it| it.query_kind.is_some())
                    .map(|it| it.name.as_str()),
            );
            it
        };
        let mut keys: HashMap<(&str, Option<&str>, &str), (u64, i64, i64), Global> =
            HashMap::new_in(Global);
        for (i, it) in model.spans.iter().enumerate() {
            let Some(function) = it.function.as_deref() else { continue };
            if !(it.name.ends_with("_query") || queries.contains(it.name.as_str())) {
                continue;
            }
            let entry =
                keys.entry((it.name.as_str(), it.krate.as_deref(), function)).or_insert((0, 0, 0));
            let running = model.span_time[i].totals.running_ns;
            if entry.0 == 0 {
                entry.2 = running;
            }
            entry.0 += 1;
            entry.1 += running;
        }
        let mut by_query: HashMap<&str, (u64, u64, i64, Vec<(String, i64)>), Global> =
            HashMap::new_in(Global);
        for ((name, krate, function), (runs, running, first)) in keys {
            if runs < 2 {
                continue;
            }
            let it = by_query.entry(name).or_default();
            it.0 += 1;
            it.1 += runs - 1;
            it.2 += running - first;
            // alloc: an example, once a key a report
            it.3.push((
                format!(
                    "{}{function} x{runs}",
                    krate.map_or(String::new(), |it| format!("{it}: "))
                ),
                running - first,
            ));
        }
        let parallelism = self.parallelism();
        for (name, (keys, extra, ns, mut examples)) in by_query {
            let examples = top(&mut examples);
            out.push(Finding {
                stacks: Stacks::default(),
                kind: Kind::Redone,
                fix: Fix::KeyForReuse,
                saves_ns: (ns as f64 / parallelism) as i64,
                estimate: Estimate::ThreadTime,
                title: format!(
                    "redone: `{name}` ran {extra} extra times over {keys} keys, {} of running",
                    s(ns)
                ),
                // alloc: a finding's subject
                subject: name.to_owned(),
                owner: self.owner(name, ""),
                evidence: vec![
                    format!("most redone: {}", list(&examples)),
                    format!("the build's mean parallelism is {parallelism:.1} workers"),
                ],
            });
        }
    }

    /// The query engine's own counts: runs in a view already run in, and runs for an evicted
    /// value.
    fn query_counts(&self, out: &mut Vec<Finding>) {
        let parallelism = self.parallelism();
        for it in &self.facts.query_counts {
            let kind = self.facts.span_kinds.iter().find(|kind| {
                kind.query_kind.as_deref() == Some(it.query.as_str())
                    || kind.name == it.query
                    || it
                        .query
                        .strip_suffix(kind.name.as_str())
                        .is_some_and(|rest| rest.ends_with("::"))
                    || kind
                        .name
                        .strip_suffix("_query")
                        .is_some_and(|name| it.query.rsplit("::").next() == Some(name))
            });
            let per_run =
                kind.filter(|it| it.runs > 0).map(|it| it.self_cpu_ns as f64 / it.runs as f64);
            let cost =
                |runs: u64| per_run.map_or(0, |per| (per * runs as f64 / parallelism) as i64);
            let estimate = if per_run.is_some() { Estimate::ThreadTime } else { Estimate::Unknown };
            let per = per_run.map_or_else(
                || "its spans count no CPU, so its cost is not known".to_owned(),
                |per| format!("{:.1} µs of CPU a run, by its spans", per / 1e3),
            );
            if it.evicted_asked > 0 {
                out.push(Finding {
                    stacks: Stacks::default(),
                    kind: Kind::Evicted,
                    fix: Fix::KeepInMemory,
                    saves_ns: cost(it.evicted_asked),
                    estimate,
                    title: format!("evicted: `{}` ran {} times for values evicted earlier in the build", it.query, it.evicted_asked),
                    // clone: a finding's subject
                    subject: it.query.clone(),
                    owner: Some("blaze_query".to_owned()),
                    evidence: vec![
                        format!(
                            "{} runs, {} verified; {} values let go under their tag's budget, {} dropped",
                            it.executed, it.verified, it.evicted, it.dropped
                        ),
                        // clone: a line of evidence, also the next finding's
                        per.clone(),
                    ],
                });
            }
            if it.reran > 0 {
                out.push(Finding {
                    stacks: Stacks::default(),
                    kind: Kind::Reran,
                    fix: Fix::KeyForReuse,
                    saves_ns: cost(it.reran),
                    estimate,
                    title: format!(
                        "reran: `{}` ran {} times in a view its row had already run in",
                        it.query, it.reran
                    ),
                    // clone: a finding's subject
                    subject: it.query.clone(),
                    owner: Some("blaze_query".to_owned()),
                    evidence: vec![format!("{} runs, {} verified", it.executed, it.verified), per],
                });
            }
            if it.rereads() {
                let ratio = it.reads_recorded as f64 / it.reads_kept.max(1) as f64;
                out.push(Finding {
                    stacks: Stacks::default(),
                    kind: Kind::Reread,
                    fix: Fix::ReadOnce,
                    saves_ns: 0,
                    estimate: Estimate::Unknown,
                    title: format!(
                        "reread: `{}` recorded {} reads over {} runs, keeping {} ({ratio:.1} reads a row kept); one run held {}",
                        it.query, it.reads_recorded, it.executed, it.reads_kept, it.reads_most
                    ),
                    // clone: a finding's subject
                    subject: it.query.clone(),
                    owner: Some("blaze_query".to_owned()),
                    evidence: vec![format!(
                        "flagged past {} reads one run or {} reads recorded a read kept (with {} recorded)",
                        QueryCount::MANY_READS,
                        QueryCount::REREAD_RATIO,
                        QueryCount::MANY_READS
                    )],
                });
            }
        }
    }

    /// The misses table, by step and cause; a first build's misses have nothing to reuse.
    fn missed(&self, out: &mut Vec<Finding>) {
        for it in self.facts.misses.iter().filter(|it| it.cause != "first") {
            // alloc: a finding's evidence
            let mut evidence = vec![format!("for example {}", it.example)];
            if let Some(what) = &it.what {
                evidence.push(format!("the commonest reason: {what}"));
            }
            out.push(Finding {
                stacks: Stacks::default(),
                kind: Kind::Missed,
                fix: Fix::ExplainMiss,
                saves_ns: 0,
                estimate: Estimate::Unknown,
                title: format!("computed again: {} `{}` ({})", it.count, it.step, it.cause),
                subject: format!("{} {}", it.step, it.cause),
                owner: None,
                evidence,
            });
        }
    }

    /// The daemon's heap fell by most of itself between the build before and this one.
    fn shed(&self, out: &mut Vec<Finding>) {
        let memory = self.facts.memory;
        // What the daemon held coming in (as the build before ended, or as this one began), against
        // the least it held during this one: the database let go between builds or at a build's
        // start is computed again.
        let Some(least) = memory.live_least else { return };
        let Some(before) = memory.live_before.max(memory.live_first) else { return };
        let first = least;
        const MIB: f64 = 1024.0 * 1024.0;
        // Shed: more than a GiB, or more than half of a heap of 256 MiB or more.
        let dropped = before - first;
        if dropped <= 1 << 30 && (dropped < 256 << 20 || first * 2 > before) {
            return;
        }
        out.push(Finding {
            stacks: Stacks::default(),
            kind: Kind::Shed,
            fix: Fix::KeepAcrossBuilds,
            saves_ns: 0,
            estimate: Estimate::Unknown,
            subject: "live bytes".to_owned(),
            owner: Some("blaze_cargo::serve".to_owned()),
            title: format!(
                "shed: the daemon held {:.0} MiB coming into the build and fell to {:.0} MiB during it",
                before as f64 / MIB,
                first as f64 / MIB
            ),
            evidence: vec![format!(
                "what it let go was computed again in this build; its peak was {:.0} MiB live, {:.0} MiB footprint",
                memory.live_peak.unwrap_or(0) as f64 / MIB,
                memory.footprint_peak.unwrap_or(0) as f64 / MIB
            )],
        });
    }

    fn blind_spots(&self, out: &mut Vec<BlindSpot>) {
        let model = self.model;
        // The path's lost time.
        let mut lost: HashMap<String, i64, Global> = HashMap::new_in(Global);
        for link in self.path.links.iter().filter(|it| it.kind == LinkKind::Unrecorded) {
            let lane = &model.lanes[link.lane];
            let why = match lane.intervals[link.interval].state {
                State::Unrecorded { why } => why.name(),
                _ => "unrecorded",
            };
            *lost.entry(format!("{} on {}", why, lane.id)).or_default() += link.dur_ns();
        }
        let lost_ns: i64 = lost.values().sum();
        if lost_ns > 0 {
            // alloc: once a report
            let mut lost: Vec<(String, i64)> = lost.into_iter().collect();
            out.push(BlindSpot {
                kind: BlindKind::Unrecorded,
                hidden_ns: lost_ns,
                subject: "critical path".to_owned(),
                title: format!("the record lost {} of the critical path ({:.0}%)", s(lost_ns), pct(lost_ns, self.path.len_ns())),
                evidence: vec![format!("by reason and lane: {}", list(&top(&mut lost)))],
                fix: "between_spans: put a span over the thread's work there; cut or no_end: the recorder's ring lost switches, so keep more or flush sooner".to_owned(),
            });
        }
        // Workers running a job inside no span. Past the bound, a job whose root span was under
        // the floor has none in the model: it is not judged, only counted for `spans_cut`.
        let cut = &self.facts.spans;
        let mut uncovered = 0i64;
        let (mut unread_jobs, mut unread_ns) = (0u64, 0i64);
        // alloc: once a report
        let mut lanes: Vec<(String, i64)> = Vec::new();
        for (l, lane) in model.lanes.iter().enumerate() {
            if lane.id.job().is_none() {
                continue;
            }
            let mut roots = lane
                .spans
                .iter()
                .filter(|&&i| model.parent[i].is_none_or(|p| model.span_time[p].lane != Some(l)))
                .peekable();
            if cut.floor_ns > 0 && roots.peek().is_none() {
                unread_jobs += 1;
                unread_ns += lane.totals().running_ns;
                continue;
            }
            let roots: i64 = roots.map(|&i| model.span_time[i].totals.running_ns).sum();
            let it = (lane.totals().running_ns - roots).max(0);
            if it > 0 {
                uncovered += it;
                lanes.push((lane.id.to_string(), it));
            }
        }
        let capacity = self.n_workers() * self.wall();
        if pct(uncovered, capacity) >= crate::gate::UNCOVERED_MAX_PCT {
            out.push(BlindSpot {
                kind: BlindKind::Uncovered,
                hidden_ns: uncovered,
                subject: "workers".to_owned(),
                title: format!(
                    "workers ran jobs inside no span for {} ({:.1}% of workers x wall; the gate allows {:.0}%)",
                    s(uncovered),
                    pct(uncovered, capacity),
                    crate::gate::UNCOVERED_MAX_PCT
                ),
                evidence: vec![format!("most: {}", list(&top(&mut lanes)))],
                fix: "every job opens its own `job` span (blaze_graph::pool::span_jobs); find the job kind and span its work".to_owned(),
            });
        }
        // CPU the spans did not count.
        let running: i64 = self.running.iter().flatten().map(|it| it.1 - it.0).sum();
        let cpu: i64 = self.facts.span_kinds.iter().map(|it| it.self_cpu_ns).sum();
        let uncounted: u64 = self.facts.thread_cpu.iter().map(|it| it.uncounted).sum();
        if running > 0 && cpu * 2 < running {
            let mut evidence = vec![format!(
                "the pool's switches say workers ran {}; spans counted {} of CPU ({:.0}%)",
                s(running),
                s(cpu),
                pct(cpu, running)
            )];
            if uncounted > 0 {
                evidence.push(format!("{uncounted} spans have no CPU (closed under another job's clock, or not counted)"));
            }
            let fix = match (self.facts.build.daemon_user_ns, self.facts.build.daemon_system_ns) {
                (Some(user), Some(system)) => {
                    let os = user + system;
                    evidence.push(format!(
                        "the OS says the daemon spent {} of CPU over the build ({} user, {} system); spans counted {:.0}% of it",
                        s(os),
                        s(user),
                        s(system),
                        pct(cpu, os)
                    ));
                    match cpu * 2 < os {
                        true => {
                            "the OS saw CPU no span counted: find the threads or jobs whose spans don't fill `self_cpu_ns` (a span closed on another job's clock, a thread with no ring)"
                        }
                        false => {
                            "the spans count most of what the OS saw: the rest of the switches' running time is starvation (see starved workers), not CPU"
                        }
                    }
                }
                _ => {
                    evidence.push("the build's row has no OS CPU for the daemon (a blaze before `daemon_user_ns`), so the spans' total can't be checked against it".to_owned());
                    "a blaze that writes `builds.daemon_user_ns` checks the spans' sum against the OS; the rest is starvation (see starved workers) or CPU outside spans"
                }
            };
            out.push(BlindSpot {
                kind: BlindKind::CpuGap,
                hidden_ns: (running - cpu).max(0),
                subject: "self_cpu_ns".to_owned(),
                title: format!(
                    "workers ran {} that no span counted CPU for",
                    s((running - cpu).max(0))
                ),
                evidence,
                fix: fix.to_owned(),
            });
        }
        let jobs = report::job_totals(model);
        if jobs.parked_ns == 0 && jobs.queued_ns == 0 && jobs.parked_or_queued_ns > 0 {
            out.push(BlindSpot {
                kind: BlindKind::NoReadyRows,
                hidden_ns: jobs.parked_or_queued_ns,
                subject: "switches.ready".to_owned(),
                title: format!("{} of waiting can't be told parked from queued: the record has no `ready` rows", s(jobs.parked_or_queued_ns)),
                evidence: Vec::new(),
                fix: "a binary that writes `ready` switches (schema 2) says which, and who woke each wait".to_owned(),
            });
        }
        let explained: u64 = self.facts.misses.iter().map(|it| it.count).sum();
        if let Some(counted) = self.facts.build.misses
            && counted > explained
        {
            out.push(BlindSpot {
                kind: BlindKind::MissesUnexplained,
                hidden_ns: 0,
                subject: "misses".to_owned(),
                title: format!("the build counted {counted} misses and the misses table explains {explained}"),
                evidence: Vec::new(),
                fix: "give every node that ran its task a misses row with its cause, or define builds.misses as what the table counts".to_owned(),
            });
        }
        let r = self.facts.recorder;
        if r.lost() > 0 {
            let (skipped, behind) = (r.rows_skipped.unwrap_or(0), r.rows_behind.unwrap_or(0));
            out.push(BlindSpot {
                kind: BlindKind::RowsLost,
                hidden_ns: 0,
                subject: "recorder".to_owned(),
                title: format!("the recorder lost {} rows in the build's window: every total here is short", r.lost()),
                evidence: vec![
                    format!(
                        "{behind} rows skipped while the history's writer was behind (`profile rows skipped behind`): its backlog of {} rows was full",
                        crate::history::BACKLOG_ROWS
                    ),
                    format!(
                        "{} rows a thread's ring of {} wrote over between the drain's reads every {:?} (`profile rows skipped`)",
                        skipped - behind,
                        crate::flight::CAP,
                        crate::history::DRAIN
                    ),
                    format!("{} spans the recorder had no slot to open (`profile spans lost`)", r.spans_lost.unwrap_or(0)),
                ],
                fix: "behind: make the history's flush faster or its backlog larger; wrapped: a bigger `flight::CAP` or a shorter drain; spans: find what held its open slots".to_owned(),
            });
        }
        if cut.floor_ns > 0 {
            // alloc: a blind spot's evidence
            let mut evidence = vec![format!(
                "{} spans not read for the whole build, {} between them (a nested span's time is its parent's too)",
                cut.cut(),
                s(cut.cut_ns)
            )];
            if let Some(it) = cut.path {
                let unread = match it.lanes_unread {
                    0 => String::new(),
                    // alloc: a cell
                    n => format!(
                        "; {n} lanes on the final path were not, and hold the spans past the floor alone"
                    ),
                };
                evidence.push(format!(
                    "the {} lanes the critical path crossed were read again down to {:.3} s: {} spans more{unread}",
                    it.lanes,
                    it.floor_ns as f64 / 1e9,
                    it.spans
                ));
            }
            for total in &cut.totals {
                // alloc: a line of evidence's parts
                let most: Vec<String> = cut
                    .groups
                    .iter()
                    .filter(|it| it.name == total.name)
                    .take(EXAMPLES)
                    .map(|it| {
                        format!(
                            "{} ({} spans, {} summed, longest {})",
                            cut_label(it),
                            it.spans,
                            s(it.dur_ns),
                            s(it.max_ns)
                        )
                    })
                    .collect();
                evidence.push(format!(
                    "{} `{}` spans under the floor, summed in DuckDB: {} in all, the longest {}, {} of CPU; most time: {}",
                    total.spans,
                    total.name,
                    s(total.dur_ns),
                    s(total.max_ns),
                    s(total.self_cpu_ns),
                    if most.is_empty() { "none named".to_owned() } else { most.join(", ") }
                ));
            }
            if unread_jobs > 0 {
                evidence.push(format!(
                    "{unread_jobs} jobs ran {} with no span read (their root span was under the floor): not judged for running inside no span",
                    s(unread_ns)
                ));
            }
            evidence.push("a park whose wait span was under the floor is classed `<kind> wait under the read floor`, not as an unnamed wait".to_owned());
            evidence.push("the spans by name count every span, read or not, and those the recorder folded (`folded`); the workers' CPU counts every span".to_owned());
            out.push(BlindSpot {
                kind: BlindKind::SpansCut,
                hidden_ns: 0,
                subject: "spans".to_owned(),
                title: format!(
                    "the model read {} of the build's {} spans, past its bound of {}: those under {:.3} s only on the critical path's lanes, the job and wait spans among them summed",
                    cut.read,
                    cut.spans,
                    crate::time::load::SPAN_BUDGET,
                    cut.floor_ns as f64 / 1e9
                ),
                evidence,
                fix: "to see one span under the floor, query the build's `spans` and `folded` (`span_sums`) with `blaze debug sql`, filtered on a group's name and subject above".to_owned(),
            });
        }
        let a = model.anomalies;
        if a.orphans + a.runs_cut + a.spans_of_no_lane + a.misnested + a.creators_unread > 0 {
            out.push(BlindSpot {
                kind: BlindKind::Misaligned,
                hidden_ns: 0,
                subject: "rows".to_owned(),
                title: format!(
                    "rows that did not line up: {} spans with no parent row, {} past their parent, {} runs cut, {} job spans of no job, {} jobs whose creator span has no row",
                    a.orphans, a.misnested, a.runs_cut, a.spans_of_no_lane, a.creators_unread
                ),
                evidence: Vec::new(),
                fix: "the recorder's rings wrote over rows before they were flushed: flush sooner or keep more".to_owned(),
            });
        }
        let start_gap = self.path.start_ns().map_or(0, |it| it - model.bounds.start_ns);
        match self.path.reached {
            Reached::Fuel { steps } => out.push(BlindSpot {
                kind: BlindKind::PathCut,
                hidden_ns: start_gap,
                subject: "critical path".to_owned(),
                title: format!("the critical path stopped at its bound of {steps} steps"),
                evidence: Vec::new(),
                fix: "the walk's bound is the model's intervals: report it as a bug in time::path"
                    .to_owned(),
            }),
            Reached::LaneStart { lane } if start_gap * 20 > self.wall() => out.push(BlindSpot {
                kind: BlindKind::PathCut,
                hidden_ns: start_gap,
                subject: "critical path".to_owned(),
                title: format!(
                    "the critical path stops {} after the build's start, at the start of {}",
                    s(start_gap),
                    model.lanes[lane].id
                ),
                evidence: vec!["the record names no job that gave it to the pool".to_owned()],
                fix: "write `by_job` on the `started` or `ran` row of every job given to the pool"
                    .to_owned(),
            }),
            _ => {}
        }
    }

    fn summary(&self, smaller: u32, cut_ns: i64) -> Summary {
        let model = self.model;
        let busy_ns: i64 = self.running.iter().flatten().map(|it| it.1 - it.0).sum();
        let path_by_link = LinkKind::ALL
            .iter()
            .map(|kind| Named {
                // alloc: a name
                name: kind.name().to_owned(),
                ns: self.path.links.iter().filter(|it| it.kind == *kind).map(Link::dur_ns).sum(),
            })
            .filter(|it| it.ns > 0)
            .collect();
        let mut phases: HashMap<&str, i64, Global> = HashMap::new_in(Global);
        for &(from, to, span, _) in &self.path_ran {
            let name =
                span.map_or("inside no span", |it| model.spans[model.root_of(it)].name.as_str());
            *phases.entry(name).or_default() += to - from;
        }
        // alloc: once a report
        let mut path_by_phase: Vec<Named> =
            phases.into_iter().map(|(name, ns)| Named { name: name.to_owned(), ns }).collect();
        path_by_phase.sort_by(|a, b| b.ns.cmp(&a.ns).then_with(|| a.name.cmp(&b.name)));
        path_by_phase.truncate(6);
        let reached = match self.path.reached {
            Reached::BuildStart => "the build's start".to_owned(),
            Reached::LaneStart { lane } => format!("the start of {}", model.lanes[lane].id),
            Reached::Fuel { steps } => format!("its bound of {steps} steps"),
            Reached::NoRun => "nothing: no job ran".to_owned(),
        };
        Summary {
            wall_ns: self.wall(),
            workers: self.workers.len() as u32,
            busy_ns,
            busy_ratio: busy_ns as f64 / (self.n_workers() * self.wall()).max(1) as f64,
            span_cpu_ns: self.facts.span_kinds.iter().map(|it| it.self_cpu_ns).sum(),
            path_ns: self.path.len_ns(),
            path_reached: reached,
            path_by_link,
            path_by_phase,
            smaller,
            cut_ns,
            recorder: self.facts.recorder,
        }
    }
}

/// `stretches` sorted and their overlaps merged.
fn merge(stretches: &mut Vec<(i64, i64)>) {
    stretches.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(stretches.len());
    for &(from, to) in stretches.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *stretches = out;
}

/// How many workers were busy over time: a step function, integrated.
#[derive(Debug, Default)]
struct Steps {
    /// Each instant the level changes, sorted.
    at: Vec<i64>,
    /// The level from `at[i]` to `at[i + 1]`.
    level: Vec<i64>,
    /// The integral from `at[0]` to `at[i]`.
    before: Vec<i128>,
}

impl Steps {
    fn of<'s>(stretches: impl Iterator<Item = &'s [(i64, i64)]>) -> Steps {
        // alloc: once a report
        let mut edges: Vec<(i64, i64)> = Vec::new();
        for it in stretches {
            for &(from, to) in it {
                edges.push((from, 1));
                edges.push((to, -1));
            }
        }
        edges.sort_unstable();
        let mut steps = Steps::default();
        let mut level = 0;
        for (t, delta) in edges {
            level += delta;
            match steps.at.last() {
                Some(&last) if last == t => *steps.level.last_mut().unwrap_or(&mut 0) = level,
                _ => {
                    let before = match (steps.at.last(), steps.level.last(), steps.before.last()) {
                        (Some(&last), Some(&was), Some(&acc)) => {
                            acc + i128::from(was) * i128::from(t - last)
                        }
                        _ => 0,
                    };
                    steps.at.push(t);
                    steps.level.push(level);
                    steps.before.push(before);
                }
            }
        }
        steps
    }

    /// The integral up to `t`.
    fn upto(&self, t: i64) -> i128 {
        let i = self.at.partition_point(|&it| it <= t);
        match i.checked_sub(1) {
            None => 0,
            Some(i) => self.before[i] + i128::from(self.level[i]) * i128::from(t - self.at[i]),
        }
    }

    /// Busy worker-nanoseconds from `from` to `to`.
    fn integral(&self, from: i64, to: i64) -> i64 {
        if to <= from {
            return 0;
        }
        i64::try_from(self.upto(to) - self.upto(from)).unwrap_or(i64::MAX)
    }
}

/// What `lane` was doing at `t`: the innermost span of the interval there, in its outermost.
fn doing_at(model: &Model, lane: usize, t: i64) -> String {
    let lane_ = &model.lanes[lane];
    let span = lane_
        .at_or_before(t)
        .and_then(|i| lane_.intervals[i].inside)
        .or_else(|| lane_.spans.iter().copied().find(|&i| model.parent[i].is_none()));
    let Some(span) = span else {
        // alloc: a label
        return lane_.id.to_string();
    };
    let root = model.root_of(span);
    let label = |i: usize| -> String {
        let it = &model.spans[i];
        match (it.name.as_str(), subject_of(model, i)) {
            // A `job` span names its closure's whole type: say the code it runs instead.
            // alloc: a label
            ("job", Some(function)) => format!("job of {}", code_in(function)),
            // alloc: a label
            (name, Some(of)) => format!("{name} {of}"),
            // alloc: a label
            (name, None) => name.to_owned(),
        }
    };
    match root == span {
        true => label(span),
        // alloc: a label
        false => format!("{} in {}", label(span), model.spans[root].name),
    }
}

/// A group of job or wait spans under the floor ([`crate::time::CutGroup`]) as a reader says it:
/// `job of hir_def::nameres::collect in hir_def`, `wait on summary_in(serde)`.
fn cut_label(group: &crate::time::CutGroup) -> String {
    let what = match (group.name.as_str(), group.subject.as_deref()) {
        // alloc: a label
        (crate::time::JOB_SPAN, Some(function)) => format!("job of {}", code_in(function)),
        // alloc: a label
        (WAIT_SPAN, Some(awaits)) => format!("wait on {awaits}"),
        // alloc: a label
        (name, Some(subject)) => format!("{name} {subject}"),
        // alloc: a label
        (name, None) => format!("{name} of nothing named"),
    };
    match &group.krate {
        // alloc: a label
        Some(krate) => format!("{what} in {krate}"),
        None => what,
    }
}

/// `class` with the package and version it names taken out, and them: `scope of unit codegen
/// serde v1.0.214 (build script)` is `scope of unit codegen (build script)`, of `serde v1.0.214`.
fn by_package(class: String) -> (String, Option<String>) {
    let words: Vec<&str> = class.split(' ').collect();
    let version = |it: &str| {
        it.strip_prefix('v')
            .is_some_and(|it| it.starts_with(|c: char| c.is_ascii_digit()) && it.contains('.'))
    };
    match words.iter().skip(1).position(|it| version(it)).map(|it| it + 1) {
        Some(at) => {
            let of = format!("{} {}", words[at - 1], words[at]);
            let rest: Vec<&str> = words
                .iter()
                .enumerate()
                .filter(|(i, _)| *i + 1 != at && *i != at)
                .map(|it| *it.1)
                .collect();
            (rest.join(" "), Some(of))
        }
        None => (class, None),
    }
}

/// What a span is about: its own subject, else its nearest ancestor's.
fn subject_of(model: &Model, mut span: usize) -> Option<&str> {
    for _ in 0..=model.spans.len() {
        if let Some(it) = model.spans[span].subject() {
            return Some(it);
        }
        span = model.parent[span]?;
    }
    None
}

/// The first path in a type name that is not the scheduler's or the query engine's plumbing:
/// `hir_def::nameres::facts::collect::expand_all` out of a pool job's closure type.
fn code_in(function: &str) -> &str {
    const PLUMBING: [&str; 8] = [
        "blaze_graph::",
        "blaze_query::",
        "base_db::db::",
        "core::",
        "std::",
        "alloc::",
        "{{closure}}",
        "impl ",
    ];
    function
        .split(['<', '>', ',', ' ', '&', '(', ')'])
        .map(|it| it.trim_end_matches("::{{closure}}").trim_matches(':'))
        .find(|it| it.contains("::") && !PLUMBING.iter().any(|p| it.starts_with(p)))
        .unwrap_or(function)
}

/// `items` summed by name, the [`EXAMPLES`] largest, largest first (ties by name).
fn top(items: &mut Vec<(String, i64)>) -> Vec<(String, i64)> {
    items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // alloc: once a finding
    let mut summed: Vec<(String, i64)> = Vec::new();
    for (name, ns) in items.drain(..) {
        match summed.last_mut() {
            Some(last) if last.0 == name => last.1 += ns,
            _ => summed.push((name, ns)),
        }
    }
    summed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let more = summed.len().saturating_sub(EXAMPLES);
    summed.truncate(EXAMPLES);
    if more > 0 {
        summed.push((format!("{more} more"), -1));
    }
    summed
}

/// `items` as `name 1.2 s, name 0.4 s, 3 more`.
fn list(items: &[(String, i64)]) -> String {
    items
        .iter()
        .map(|(name, ns)| match *ns {
            -1 => name.clone(), // clone: a cell
            ns => format!("{} {}", if name.is_empty() { "(unnamed)" } else { name }, s(ns)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A source file and line named in `text`: `crates/base-db/src/maintain.rs:161`.
fn file_in(text: &str) -> Option<String> {
    let start = text.find("crates/").or_else(|| text.find("library/"))?;
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace() || c == ')' || c == ',').unwrap_or(rest.len());
    let it = &rest[..end];
    it.contains(".rs").then(|| it.to_owned())
}
