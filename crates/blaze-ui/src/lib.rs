//! How a build is shown, from its records ([`blaze_protocol::events`]) alone: a renderer
//! is a consumer of what the daemon publishes, so the same records give the same text
//! whichever terminal attaches, and a recorded build replays into it.
//!
//! - [`Live`], on a terminal: a region at the bottom that is redrawn in place (not a full
//!   screen), with the step under way, the crates being built and their counts of
//!   functions, each with how long it has taken; what is done scrolls above it as lines
//!   that stay. It is never left unchanged for more than a tick. At the end it is cleared
//!   and one summary line takes its place.
//! - [`Plain`], elsewhere (CI, a pipe): one line per step and crate, as cargo writes them,
//!   and a line every [`HEARTBEAT_NS`] that nothing else was said.

mod live;
mod plain;
#[cfg(test)]
mod tests;

pub use live::Live;
pub use plain::Plain;

use blaze_protocol::events::{
    Cache, Cause, CrateState, Event, Fns, Level, NodeKind, NodeState, Record, SLOW_NS, Slow, Step,
    Summary, Took,
};

/// The longest a plain rendering says nothing while a step is under way.
pub const HEARTBEAT_NS: u64 = 15_000_000_000;

/// A build as its records so far have it.
#[derive(Default, Debug)]
pub struct Build {
    pub command: Option<String>,
    /// Every step begun, in order.
    pub steps: Vec<Stepped>,
    /// The nodes of the build's graph ([`Event::Node`]), each after what it waits for.
    pub graph: Vec<Noded>,
    /// Every crate of the graph, and every other the build said anything of, in the order it
    /// first did; and a row for each node of the graph that is not a crate's (a build
    /// script's, a link).
    pub crates: Vec<Crated>,
    pub cache: Option<Cache>,
    /// What was compiled again and why: name, crate, cause.
    pub rebuilt: Vec<(String, String, Cause)>,
    /// How many more were compiled again than `rebuilt` names.
    pub unlisted: u64,
    pub refused: Vec<(String, String)>,
    pub finished: Option<Finished>,
    /// The time of the last record.
    pub last_ns: u64,
    /// The longest stretch so far with no step under way, when it ended, and the step that
    /// ended before it ([`Build::unnamed`]).
    pub unnamed: Option<Unnamed>,
    /// The nodes of [`Build::graph`] that held the build up, from the first
    /// ([`Event::Critical`]).
    pub critical: Vec<usize>,
    /// How busy the worker was, in hundredths of a core, from each time on
    /// ([`Event::Cores`]), and of how many cores.
    pub cores: Vec<(u64, u32)>,
    pub cores_of: u32,
    /// The functions compiling longest as said last, each with since when
    /// ([`Event::Compiling`]).
    pub compiling: Vec<(Slow, u64)>,
    /// The functions that took longest to compile, longest first ([`Event::Slowest`]).
    pub slowest: Vec<Slow>,
    /// Since when no step is under way, and the step that ended then.
    idle: Option<(u64, Step)>,
}

/// Time a build spent with no step under way: what ran then is shown as nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unnamed {
    pub took_ns: u64,
    pub after: Step,
}

/// A stretch with no step under way longer than this is said ([`why_lines`]): every phase
/// that takes long has a name.
pub const UNNAMED_NS: u64 = 1_000_000_000;

/// A kind of step in the line of steps ([`Build::strip`]): under way or done, how long all
/// of its kind took, and the count of the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strip {
    pub step: Step,
    pub running: bool,
    pub took_ns: u64,
    pub count: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Stepped {
    pub step: Step,
    pub of: Option<String>,
    pub began_ns: u64,
    pub ended_ns: Option<u64>,
    /// The last [`Event::Progress`] of it.
    pub progress: Option<Progress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub longest: Option<String>,
}

impl Stepped {
    /// What the step is of: its crate or program, or the step's own word for the whole.
    pub fn what(&self) -> &str {
        self.of.as_deref().unwrap_or(self.step.whole())
    }

    /// `3/14 files`, or `1200 fns` while the total is not known; nothing before a count.
    pub fn count(&self) -> Option<String> {
        let progress = self.progress.as_ref()?;
        let counts = self.step.counts();
        Some(match progress.total {
            0 => format!("{} {counts}", progress.done),
            total => format!("{}/{total} {counts}", progress.done),
        })
    }
}

/// A node of the build's graph: its id, the row of [`Build::crates`] it moves, and the nodes
/// it waits for, by their index in [`Build::graph`].
#[derive(Debug, Clone)]
pub struct Noded {
    pub id: u32,
    pub kind: NodeKind,
    pub row: usize,
    pub deps: Vec<usize>,
}

/// What a node of `kind` named `name` is a row of: a crate's own, or one of its own named
/// for what it does, and the word it is under way with.
fn row_of(kind: NodeKind, name: &str) -> (String, Option<&'static str>) {
    let own = |what: &str, word| (format!("{name} {what}"), Some(word));
    match kind {
        NodeKind::Names | NodeKind::Bodies | NodeKind::Codegen | NodeKind::Std => {
            (name.to_owned(), None)
        }
        NodeKind::Script => own("build script", "compiling"),
        NodeKind::ScriptRun => own("build script", "running"),
        NodeKind::Macro => own("proc macro", "linking"),
        NodeKind::Fetch => own("source", "fetching"),
        NodeKind::Manifest => (name.to_owned(), Some("reading")),
        NodeKind::Store => own("to the store", "writing"),
        NodeKind::Link => own("link", "linking"),
        NodeKind::Test => own("tests", "running"),
    }
}

/// The state a crate's row is in once its node of `kind` is in `state`.
fn crate_state(kind: NodeKind, state: &NodeState) -> CrateState {
    match (kind, state) {
        (_, NodeState::Waiting | NodeState::Ready) => CrateState::Waiting,
        (NodeKind::Names, NodeState::Running) => CrateState::Resolving,
        (NodeKind::Bodies, NodeState::Running) => CrateState::Checking,
        (_, NodeState::Running) => CrateState::Running,
        (NodeKind::Names | NodeKind::Bodies, NodeState::Built | NodeState::Cached { .. }) => {
            CrateState::Resolved
        }
        (_, NodeState::Built) => CrateState::Built,
        (_, NodeState::Cached { .. }) => CrateState::UpToDate,
        (_, NodeState::Failed { .. } | NodeState::Skipped { .. }) => CrateState::Failed,
    }
}

#[derive(Debug, Clone)]
pub struct Crated {
    pub name: String,
    pub state: CrateState,
    /// The word it is under way with when that is not its state's: a build script's
    /// `running`.
    pub doing: Option<&'static str>,
    pub fns: Fns,
    /// When it was first other than waiting.
    pub began_ns: u64,
    pub ended_ns: Option<u64>,
    /// When it came to the state it is in.
    pub since_ns: u64,
    /// How long it was resolving, checking or compiling before `since_ns`.
    pub busy_ns: u64,
}

impl Crated {
    fn waiting(name: &str, at: u64) -> Crated {
        let (fns, state) = (Fns::default(), CrateState::Waiting);
        let name = name.to_owned();
        let doing = None;
        Crated { name, state, doing, fns, began_ns: at, ended_ns: None, since_ns: at, busy_ns: 0 }
    }

    /// Whether something is being done of it now.
    pub fn is_busy(&self) -> bool {
        busy(self.state)
    }

    /// The word for what is being done of it, or for where it is.
    pub fn word(&self) -> &'static str {
        match (self.doing, self.is_busy()) {
            (Some(word), true) => word,
            _ => self.state.word(),
        }
    }

    /// Has it in `state` from `at`, `moved` when that is another state or a further count.
    fn moved(&mut self, state: CrateState, at: u64) -> bool {
        let done = state.is_done();
        let moved = self.state != state || self.ended_ns.is_none() && done;
        if self.state == CrateState::Waiting && state != CrateState::Waiting {
            self.began_ns = at;
        }
        if self.state != state {
            if self.is_busy() {
                self.busy_ns += at.saturating_sub(self.since_ns);
            }
            self.since_ns = at;
        }
        self.state = state;
        // Compiled again in the same build: a patch that is built in full after all.
        if state == CrateState::Running {
            self.ended_ns = None;
        }
        if done && self.ended_ns.is_none() {
            self.ended_ns = Some(at);
        }
        moved
    }

    /// How long it was resolving, checking or compiling, until `now_ns`.
    pub fn busy_ns(&self, now_ns: u64) -> u64 {
        let current = if self.is_busy() { now_ns.saturating_sub(self.since_ns) } else { 0 };
        self.busy_ns + current
    }
}

fn busy(state: CrateState) -> bool {
    matches!(state, CrateState::Resolving | CrateState::Checking | CrateState::Running)
}

#[derive(Debug, Clone)]
pub struct Finished {
    pub ok: bool,
    pub summary: Summary,
    pub at_ns: u64,
    /// What held the build up, the first first, and the time from its start to the last's end
    /// ([`Build::critical_path`]).
    pub chain: Vec<String>,
    pub chain_ns: u64,
    pub unnamed: Option<Unnamed>,
    /// How many cores each of the summary's slowest steps kept busy on average, when the
    /// worker said, and of how many.
    pub cores: Vec<Option<f64>>,
    pub cores_of: u32,
    /// The functions that took longest to compile, longest first.
    pub slowest: Vec<Slow>,
}

/// What a record changed, for a renderer to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Nothing,
    /// The step at this index of [`Build::steps`] began, or ended.
    Began(usize),
    Ended(usize),
    /// The crate at this index of [`Build::crates`] is in another state, or further.
    Crate {
        at: usize,
        moved: bool,
    },
    Said(Level, String),
    Refused(usize),
    Rebuilt(usize),
    Finished,
}

impl Build {
    pub fn apply(&mut self, record: &Record) -> Change {
        let at = record.at_ns;
        self.last_ns = self.last_ns.max(at);
        match &record.event {
            Event::Began { command, .. } => {
                self.command = Some(command.clone());
                Change::Nothing
            }
            Event::Started { step, of } => {
                self.named(at);
                let of = of.clone();
                let (began_ns, ended_ns, progress) = (at, None, None);
                self.steps.push(Stepped { step: *step, of, began_ns, ended_ns, progress });
                Change::Began(self.steps.len() - 1)
            }
            Event::Progress { step, done, total, longest } => {
                let open =
                    self.steps.iter().rposition(|it| it.step == *step && it.ended_ns.is_none());
                if let Some(index) = open {
                    let longest = longest.clone();
                    self.steps[index].progress =
                        Some(Progress { done: *done, total: *total, longest });
                }
                Change::Nothing
            }
            Event::Node { id, kind, name, deps } => {
                let row = self.row(&row_of(*kind, name).0, at);
                let deps = deps.iter().filter_map(|dep| self.node(*dep)).collect();
                self.graph.push(Noded { id: *id, kind: *kind, row, deps });
                Change::Nothing
            }
            Event::Moved { id, state } => match self.node(*id) {
                Some(node) => {
                    let Noded { kind, row, .. } = self.graph[node];
                    let krate = &mut self.crates[row];
                    // A row of several nodes says what the one that moved last does.
                    krate.doing = row_of(kind, "").1;
                    let moved = krate.moved(crate_state(kind, state), at);
                    Change::Crate { at: row, moved }
                }
                None => Change::Nothing,
            },
            Event::Cache { cache } => {
                self.cache = Some(cache.clone());
                Change::Nothing
            }
            Event::Ended { step, of } => {
                let open = self
                    .steps
                    .iter()
                    .rposition(|it| it.step == *step && it.of == *of && it.ended_ns.is_none());
                match open {
                    Some(index) => {
                        self.steps[index].ended_ns = Some(at);
                        if self.running().next().is_none() {
                            self.idle = Some((at, *step));
                        }
                        Change::Ended(index)
                    }
                    None => Change::Nothing,
                }
            }
            Event::Crate { name, state, fns } => {
                let rows = self.crates.len();
                let index = self.row(name, at);
                let krate = &mut self.crates[index];
                let moved = krate.moved(*state, at) || index == rows;
                krate.fns = *fns;
                Change::Crate { at: index, moved }
            }
            Event::Rebuilt { name, krate, cause } => {
                self.rebuilt.push((name.clone(), krate.clone(), cause.clone()));
                Change::Rebuilt(self.rebuilt.len() - 1)
            }
            Event::Unlisted { count } => {
                self.unlisted += count;
                Change::Nothing
            }
            Event::Refused { name, why } => {
                self.refused.push((name.clone(), why.clone()));
                Change::Refused(self.refused.len() - 1)
            }
            Event::Said { level, text } => Change::Said(*level, text.clone()),
            Event::Finished { ok, summary } => {
                self.named(at);
                let (chain, chain_ns) = self.critical_path(at);
                let chain = chain.iter().rev().map(|&it| self.crates[it].name.clone()).collect();
                let (ok, summary, unnamed) = (*ok, summary.clone(), self.unnamed);
                let cores = summary.slowest.iter().map(|it| self.busy_in(it, at)).collect();
                let cores_of = self.cores_of;
                self.compiling.clear();
                let slowest = self.slowest.clone();
                self.finished = Some(Finished {
                    ok,
                    summary,
                    at_ns: at,
                    chain,
                    chain_ns,
                    unnamed,
                    cores,
                    cores_of,
                    slowest,
                });
                Change::Finished
            }
            Event::Critical { path } => {
                self.critical = path.iter().filter_map(|&it| self.node(it)).collect();
                Change::Nothing
            }
            Event::Cores { busy, of } => {
                self.cores.push((at, *busy));
                self.cores_of = *of;
                Change::Nothing
            }
            Event::Compiling { functions } => {
                let since = |it: &Slow| at.saturating_sub(it.ns);
                self.compiling = functions.iter().map(|it| (it.clone(), since(it))).collect();
                Change::Nothing
            }
            Event::Slowest { functions } => {
                self.slowest.clone_from(functions);
                Change::Nothing
            }
            // The build page's (docs/web.md).
            Event::Functions { .. } => Change::Nothing,
        }
    }

    /// The functions of crate `krate` compiling for [`SLOW_NS`] or more by `now_ns`, longest
    /// first: their names and for how long.
    pub fn compiling_of(&self, krate: &str, now_ns: u64) -> Vec<(&str, u64)> {
        let of = self.compiling.iter().filter(|(it, _)| it.krate == krate);
        let of = of.map(|(it, since)| (it.name.as_str(), now_ns.saturating_sub(*since)));
        let mut of: Vec<_> = of.filter(|it| it.1 >= SLOW_NS).collect();
        of.sort_by_key(|it| std::cmp::Reverse(it.1));
        of
    }

    /// A step began, or the build ended, at `at`: the time since the last step ended went
    /// unnamed.
    fn named(&mut self, at: u64) {
        let Some((since, after)) = self.idle.take() else { return };
        let took_ns = at.saturating_sub(since);
        if self.unnamed.is_none_or(|it| took_ns > it.took_ns) {
            self.unnamed = Some(Unnamed { took_ns, after });
        }
    }

    /// Each kind of step begun, in the order it first began: whether one of it is under way,
    /// and how long they took by `now_ns`. Done ones too quick to matter are left out.
    pub fn strip(&self, now_ns: u64) -> Vec<Strip> {
        let mut strip: Vec<Strip> = Vec::new();
        for stepped in &self.steps {
            let took_ns = stepped.ended_ns.unwrap_or(now_ns).saturating_sub(stepped.began_ns);
            let running = stepped.ended_ns.is_none();
            let count = stepped.count();
            match strip.iter_mut().find(|it| it.step == stepped.step) {
                Some(it) => {
                    it.took_ns += took_ns;
                    it.running |= running;
                    it.count = count.or(it.count.take());
                }
                None => strip.push(Strip { step: stepped.step, running, took_ns, count }),
            }
        }
        strip.retain(|it| it.running || it.took_ns >= 50_000_000);
        strip
    }

    /// Every step in the order it began, and the time spent at no step between them, as
    /// `blaze why` lists them by `now_ns`: what it was, how long it took, and how many cores
    /// the worker kept busy meanwhile.
    pub fn why_steps(&self, now_ns: u64) -> Vec<String> {
        // Shorter steps say nothing of where the time went.
        const GAP_NS: u64 = 50_000_000;
        const STEP_NS: u64 = 10_000_000;
        let cores = |from: u64, to: u64| match self.busy_between(from, to) {
            Some((busy, _)) => format!("  {busy:.1}/{} cores", self.cores_of),
            None => String::new(),
        };
        let mut rows: Vec<(String, u64, String)> = Vec::new();
        let mut free = 0;
        for stepped in &self.steps {
            if stepped.began_ns.saturating_sub(free) >= GAP_NS {
                let gap = stepped.began_ns - free;
                rows.push(("at no step".into(), gap, cores(free, stepped.began_ns)));
            }
            let ended = stepped.ended_ns.unwrap_or(now_ns);
            free = free.max(ended);
            let took_ns = ended.saturating_sub(stepped.began_ns);
            if took_ns < STEP_NS {
                continue;
            }
            let mut what = short(stepped.step, stepped.of.as_deref());
            // `check 74 files` has its count already.
            let of = stepped.of.as_deref().and_then(|it| it.rsplit(' ').next());
            if let Some(count) = stepped.count().filter(|it| it.rsplit(' ').next() != of) {
                what = format!("{what} {count}");
            }
            rows.push((what, took_ns, cores(stepped.began_ns, ended)));
        }
        if !rows.is_empty() && now_ns.saturating_sub(free) >= GAP_NS {
            rows.push(("at no step".into(), now_ns - free, cores(free, now_ns)));
        }
        let width = rows.iter().map(|it| it.0.chars().count()).max().unwrap_or(0);
        rows.iter()
            .map(|(what, ns, cores)| format!("{what:<width$}  {:>6}{cores}", took(*ns)))
            .collect()
    }

    /// How many cores the worker kept busy at `at`, in hundredths: as it said last before.
    pub fn busy_at(&self, at: u64) -> Option<u32> {
        let after = self.cores.partition_point(|it| it.0 <= at);
        after.checked_sub(1).map(|it| self.cores[it].1)
    }

    /// How many cores the worker kept busy on average from `from` to `to`, when it said.
    fn busy_between(&self, from: u64, to: u64) -> Option<(f64, u64)> {
        let first = self.busy_at(from).or_else(|| self.cores.first().map(|it| it.1))?;
        let (mut sum, mut at, mut level) = (0.0, from, first);
        for &(when, busy) in self.cores.iter().filter(|it| it.0 > from && it.0 < to) {
            sum += f64::from(level) * (when - at) as f64;
            (at, level) = (when, busy);
        }
        sum += f64::from(level) * to.saturating_sub(at) as f64;
        let spent = to.saturating_sub(from);
        (spent > 0).then(|| (sum / spent as f64 / 100.0, spent))
    }

    /// How many cores the steps `took` adds up kept busy on average, by `now_ns`.
    fn busy_in(&self, took: &Took, now_ns: u64) -> Option<f64> {
        let (mut sum, mut spent) = (0.0, 0);
        for stepped in self
            .steps
            .iter()
            .filter(|it| it.step == took.step && (it.step == Step::Codegen || it.of == took.of))
        {
            let ended = stepped.ended_ns.unwrap_or(now_ns);
            if let Some((busy, ns)) = self.busy_between(stepped.began_ns, ended) {
                sum += busy * ns as f64;
                spent += ns;
            }
        }
        (spent > 0).then(|| sum / spent as f64)
    }

    /// The row of [`Build::crates`] named `name`, a new one waiting from `at` if none is.
    fn row(&mut self, name: &str, at: u64) -> usize {
        match self.crates.iter().position(|it| it.name == name) {
            Some(row) => row,
            None => {
                self.crates.push(Crated::waiting(name, at));
                self.crates.len() - 1
            }
        }
    }

    /// The index in [`Build::graph`] of the node said last with `id`.
    fn node(&self, id: u32) -> Option<usize> {
        self.graph.iter().rposition(|it| it.id == id)
    }

    /// The steps under way, the one begun last at the end.
    pub fn running(&self) -> impl Iterator<Item = &Stepped> {
        self.steps.iter().filter(|it| it.ended_ns.is_none())
    }

    /// What the records so far come to, for the last line of a build that took `took_ns`:
    /// the first few rebuilt, and a count of the rest.
    pub fn summary(&self, took_ns: u64, error: Option<String>) -> Summary {
        const LISTED: usize = 8;
        let rebuilt = self.rebuilt.iter().take(LISTED);
        let rebuilt = rebuilt.map(|(name, _, cause)| (name.clone(), cause.clone())).collect();
        let more = self.rebuilt.len().saturating_sub(LISTED) as u64 + self.unlisted;
        let crates = self.crates.iter().filter(|it| it.state != CrateState::Waiting).count();
        let (fns, cache, slowest) = (self.fns(), self.cache.clone(), self.slowest(took_ns));
        Summary { took_ns, crates: crates as u64, fns, rebuilt, more, error, cache, slowest }
    }

    /// The steps that took longest by `now_ns`, longest first: each step of the same thing
    /// once, its times added up (the codegen of every crate is one), and only those that took
    /// a part of the build worth naming.
    pub fn slowest(&self, now_ns: u64) -> Vec<Took> {
        const LISTED: usize = 3;
        let mut by_step: Vec<Took> = Vec::new();
        for stepped in &self.steps {
            let took_ns = stepped.ended_ns.unwrap_or(now_ns).saturating_sub(stepped.began_ns);
            let same = |it: &&mut Took| {
                it.step == stepped.step && (it.step == Step::Codegen || it.of == stepped.of)
            };
            match by_step.iter_mut().find(same) {
                Some(took) => {
                    took.took_ns += took_ns;
                    if took.of != stepped.of {
                        took.of = None;
                    }
                }
                None => by_step.push(Took { step: stepped.step, of: stepped.of.clone(), took_ns }),
            }
        }
        // Longest first; of the same length, the one begun first.
        by_step.sort_by_key(|it| std::cmp::Reverse(it.took_ns));
        let worth = (now_ns / 50).max(100_000_000);
        by_step.retain(|it| it.took_ns >= worth);
        by_step.truncate(LISTED);
        by_step
    }

    /// The rows on what held the build up, as the graph said it at its end
    /// ([`Event::Critical`]): the one that finished last first, back to the one it began
    /// with; and the time from that one's start to the last one's end, by `now_ns` if it is
    /// not done. Empty before the graph says it.
    pub fn critical_path(&self, now_ns: u64) -> (Vec<usize>, u64) {
        let mut chain: Vec<usize> = Vec::new();
        for &node in self.critical.iter().rev() {
            let row = self.graph[node].row;
            if !chain.contains(&row) {
                chain.push(row);
            }
        }
        let (Some(&last), Some(&first)) = (chain.first(), chain.last()) else {
            return (Vec::new(), 0);
        };
        let last = &self.crates[last];
        let ended = match (last.ended_ns, last.is_busy()) {
            (Some(ended), _) => ended,
            (None, true) => now_ns,
            // Resolved and waiting to be compiled: done with what it did last.
            (None, false) => last.since_ns,
        };
        (chain, ended.saturating_sub(self.crates[first].began_ns))
    }

    /// The functions of every crate together.
    pub fn fns(&self) -> Fns {
        self.crates.iter().fold(Fns::default(), |all, it| Fns {
            total: all.total + it.fns.total,
            done: all.done + it.fns.done,
            kept: all.kept + it.fns.kept,
        })
    }
}

/// The last `most` characters of `text`, after an ellipsis when it is longer: a function's
/// path says which it is at its end.
pub fn tail(text: &str, most: usize) -> String {
    let count = text.chars().count();
    match count > most {
        true => ['\u{2026}'].into_iter().chain(text.chars().skip(count + 1 - most)).collect(),
        false => text.to_owned(),
    }
}

/// A duration as a person reads it: `0.21s`, `33.2s`, `1m17s`.
pub fn took(ns: u64) -> String {
    let ms = ns / 1_000_000;
    match ms {
        0..=9_999 => format!("{}.{:02}s", ms / 1000, ms % 1000 / 10),
        10_000..=59_999 => format!("{}.{}s", ms / 1000, ms % 1000 / 100),
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

/// Whether a build did nothing and failed at nothing: it has no last line of its own, as cargo
/// says nothing of a fresh build but its `Finished`.
pub fn fresh(finished: &Finished) -> bool {
    let summary = &finished.summary;
    finished.ok && summary.rebuilt.is_empty() && summary.fns.compiled() == 0
}

/// A build that took longer than this says why after its last line ([`why_lines`]).
pub const WHY_NS: u64 = 1_000_000_000;

fn took_ns(finished: &Finished) -> u64 {
    if finished.summary.took_ns > 0 { finished.summary.took_ns } else { finished.at_ns }
}

/// The last lines said of a build: none of a fresh build that was quick, nor of a failed one
/// when the errors are said in full after it (`errors_said`), as the command that asked for
/// the build says them with cargo's `could not compile`, the one line of failure cargo has.
/// One that took long says why after ([`why_lines`]).
pub fn last_lines(finished: &Finished, color: bool, errors_said: bool) -> Vec<String> {
    let quick = took_ns(finished) < WHY_NS;
    if fresh(finished) && quick || !finished.ok && errors_said {
        return Vec::new();
    }
    let mut lines = vec![summary_line(finished, color)];
    lines.extend(why_lines(finished, color));
    lines
}

/// Why a build took as long as it did, for one that took longer than [`WHY_NS`]: its slowest
/// steps, what the store had of its compile, the longest chain of crates that waited for
/// each other, and time no step was named for.
///
/// ```text
///         Took 2m15s: checking 14 files 1m40s, loading workspace 10.7s, compiling functions 4.20s
///        Cache cold: the store has none of this build
///        Chain syn → serde_derive → anim 18.2s
///              `blaze why` says more
/// ```
pub fn why_lines(finished: &Finished, color: bool) -> Vec<String> {
    let summary = &finished.summary;
    let took_ns = took_ns(finished);
    if took_ns < WHY_NS || !finished.ok {
        return Vec::new();
    }
    let status =
        |word: &str, rest: &str| format!("{} {rest}", paint(GREEN, &format!("{word:>12}"), color));
    let mut lines = Vec::new();
    if !summary.slowest.is_empty() {
        let steps: Vec<String> = summary
            .slowest
            .iter()
            .enumerate()
            .map(|(at, it)| {
                let cores = match finished.cores.get(at).copied().flatten() {
                    Some(busy) if at == 0 => format!(" ({busy:.1}/{} cores)", finished.cores_of),
                    Some(busy) => format!(" ({busy:.1}/{})", finished.cores_of),
                    None => String::new(),
                };
                format!("{} {}{cores}", short(it.step, it.of.as_deref()), took(it.took_ns))
            })
            .collect();
        lines.push(status("Took", &format!("{}: {}", took(took_ns), steps.join(", "))));
    }
    if !finished.slowest.is_empty() {
        const LISTED: usize = 5;
        let listed = finished.slowest.iter().take(LISTED);
        let listed: Vec<String> =
            listed.map(|it| format!("{} {}", tail(&it.name, 48), took(it.ns))).collect();
        let more = match finished.slowest.len().saturating_sub(LISTED) {
            0 => String::new(),
            more => format!(" (and {more} more)"),
        };
        lines.push(status("Slowest", &format!("fns: {}{more}", listed.join(", "))));
    }
    // The line before says what was compiled and kept; this says what it does not.
    if let Some(cache) = &summary.cache {
        let rest = match &cache.cold {
            Some(why) => Some(format!("cold: {why}")),
            None if cache.fetched > 0 => Some(format!(
                "{} found here, {} fetched from the remote store",
                cache.found, cache.fetched
            )),
            None => None,
        };
        lines.extend(rest.map(|rest| status("Cache", &rest)));
    }
    if finished.chain.len() > 1 && finished.chain_ns > 0 {
        let chain = finished.chain.join(" \u{2192} ");
        lines.push(status("Chain", &format!("{chain} {}", took(finished.chain_ns))));
    }
    if let Some(unnamed) = finished.unnamed.filter(|it| it.took_ns >= UNNAMED_NS) {
        let after = unnamed.after.whole();
        let rest = format!("{} with no step named, after {after}", took(unnamed.took_ns));
        lines.push(said_line(Level::Warning, &rest, color));
    }
    if !lines.is_empty() {
        lines.push(paint(DIM, &format!("{:>12} `blaze why` says more", ""), color));
    }
    lines
}

/// The word of a step in the line of steps: short, as the line has all of them.
pub fn label(step: Step) -> &'static str {
    match step {
        Step::LoadWorkspace => "workspace",
        Step::Fetch => "fetch",
        Step::Resolve => "names",
        Step::FrontEnd => "types",
        Step::Codegen => "codegen",
        Step::Std => "std",
        Step::Link => "link",
        Step::Check => "check",
        Step::Run => "run",
        Step::Test => "tests",
        Step::BuildScripts => "build scripts",
        Step::Macros => "proc macros",
        Step::Optimize => "optimize",
        Step::Store => "store",
        Step::Remote => "remote",
    }
}

/// A step as the steps line names it, with what it is of: `codegen anim`, `std of build scripts`.
fn short(step: Step, of: Option<&str>) -> String {
    match of {
        None => label(step).to_owned(),
        Some(of) => match of.strip_prefix("the standard library ") {
            Some(rest) => format!("{} {rest}", label(step)),
            None => format!("{} {of}", label(step)),
        },
    }
}

/// The last line of a build: what it rebuilt and why, or why it failed, and how long it took.
/// Not cargo's `Finished`, which `blaze` says after it.
pub fn summary_line(finished: &Finished, color: bool) -> String {
    let summary = &finished.summary;
    let took = took(took_ns(finished));
    if !finished.ok {
        let why = summary.error.as_deref().unwrap_or("the build failed");
        return format!("{}: {why} (after {took})", paint(RED, "error", color));
    }
    let compiled = summary.fns.compiled();
    let (word, what) = if !summary.rebuilt.is_empty() {
        let listed = summary.rebuilt.len() as u64 + summary.more;
        let mut said = format!("{listed} {}: {}", plural(listed, "fn"), causes(&summary.rebuilt));
        if summary.more > 0 {
            said.push_str(&format!(" and {} more", summary.more));
        }
        ("Rebuilt", said)
    } else if compiled > 0 {
        let kept = summary.fns.kept;
        let crates = summary.crates;
        let said = format!(
            "{compiled} {} in {crates} {}, {kept} kept",
            plural(compiled, "fn"),
            plural(crates, "crate")
        );
        ("Compiled", said)
    } else if let Some(cache) = summary.cache.as_ref().filter(|it| it.found + it.fetched > 0) {
        let found = cache.found + cache.fetched;
        let said = format!(
            "from the store: {found} {} found, {} compiled",
            plural(found, "fn"),
            cache.compiled
        );
        ("Built", said)
    } else {
        ("Fresh", "nothing to rebuild".to_owned())
    };
    format!("{} {what}; {took}", paint(GREEN, &format!("{word:>12}"), color))
}

/// The word a step is said done with.
fn done_word(step: Step) -> &'static str {
    match step {
        Step::LoadWorkspace => "Loaded",
        Step::Fetch => "Fetched",
        Step::Resolve => "Resolved",
        Step::FrontEnd | Step::Check => "Checked",
        Step::Codegen | Step::Std => "Compiled",
        Step::Link => "Linked",
        Step::Run => "Ran",
        Step::BuildScripts => "Built",
        Step::Test => "Tested",
        Step::Macros => "Loaded",
        Step::Optimize => "Optimized",
        Step::Store => "Stored",
        Step::Remote => "Fetched",
    }
}

/// A step or crate done sooner than this is not said done in lines that stay, but for a
/// crate built on a terminal: cargo says each. Off a terminal, a step is not said begun
/// before it has gone on for this long ([`Plain`]).
const NEWS_NS: u64 = 500_000_000;

/// The lines `change` adds that stay: on a terminal (`live`) above the region, elsewhere all
/// there is.
fn staying(
    build: &Build,
    change: &Change,
    color: bool,
    live: bool,
    errors_said: bool,
) -> Vec<String> {
    let status =
        |word: &str, rest: &str| format!("{} {rest}", paint(GREEN, &format!("{word:>12}"), color));
    match change {
        Change::Ended(at) => {
            let stepped = &build.steps[*at];
            let took_ns = stepped.ended_ns.unwrap_or(stepped.began_ns) - stepped.began_ns;
            // The crates say how their compiling went.
            let said_by_crates = stepped.step == Step::Codegen && !build.crates.is_empty();
            match took_ns >= NEWS_NS && !said_by_crates {
                true => {
                    let rest = format!("{} in {}", stepped.what(), took(took_ns));
                    vec![status(done_word(stepped.step), &rest)]
                }
                false => Vec::new(),
            }
        }
        Change::Crate { at, moved: true } => {
            let krate = &build.crates[*at];
            // The time it was worked on, not that it waited between steps.
            let took_ns = krate.busy_ns(krate.ended_ns.unwrap_or(krate.since_ns));
            match krate.state {
                // The step of compiling it said so already.
                CrateState::Running
                    if !live
                        && !build.running().any(|it| {
                            it.step == Step::Codegen && it.of.as_deref() == Some(&krate.name)
                        }) =>
                {
                    vec![status("Compiling", &krate.name)]
                }
                // Of a graph's crates the tree showed each, and `blaze why` lists them.
                CrateState::Built if build.graph.is_empty() && (live || took_ns >= NEWS_NS) => {
                    let compiled = krate.fns.compiled();
                    let fns = match krate.fns.total {
                        0 => String::new(),
                        _ => format!(
                            ": {compiled} {} compiled, {} kept",
                            plural(compiled, "fn"),
                            krate.fns.kept
                        ),
                    };
                    vec![status("Compiled", &format!("{}{fns} in {}", krate.name, took(took_ns)))]
                }
                _ => Vec::new(),
            }
        }
        Change::Said(level, text) => vec![said_line(*level, text, color)],
        Change::Refused(at) => {
            let (name, why) = &build.refused[*at];
            vec![said_line(Level::Error, &format!("blaze does not build `{name}`: {why}"), color)]
        }
        Change::Finished if !live => {
            build.finished.iter().flat_map(|it| last_lines(it, color, errors_said)).collect()
        }
        _ => Vec::new(),
    }
}

/// `render (body edited), draw, tick (calls render)`: names with the same cause one after
/// another say it once, after the last of them.
fn causes(rebuilt: &[(String, Cause)]) -> String {
    let mut said = String::new();
    for (at, (name, cause)) in rebuilt.iter().enumerate() {
        if at > 0 {
            said.push_str(", ");
        }
        said.push_str(name);
        if rebuilt.get(at + 1).is_none_or(|next| next.1 != *cause) {
            said.push_str(&format!(" ({})", cause.short()));
        }
    }
    said
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 { word.to_owned() } else { format!("{word}s") }
}

const GREEN: &str = "\x1b[1m\x1b[92m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn paint(color: &str, text: &str, colored: bool) -> String {
    if colored { format!("{color}{text}{RESET}") } else { text.to_owned() }
}

/// A line of `level`, as cargo writes them.
fn said_line(level: Level, text: &str, color: bool) -> String {
    let (word, color_of) = match level {
        Level::Note => ("note", GREEN),
        Level::Warning => ("warning", YELLOW),
        Level::Error => ("error", RED),
    };
    format!("{}: {text}", paint(color_of, word, color))
}
