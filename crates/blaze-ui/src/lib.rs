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
    Cache, Cause, CrateState, Event, Fns, Level, NodeKind, NodeState, Record, Step, Summary, Took,
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
                self.finished = Some(Finished { ok: *ok, summary: summary.clone(), at_ns: at });
                Change::Finished
            }
            // The live view finds the longest chain as it goes ([`Build::critical_path`]).
            Event::Critical { .. } => Change::Nothing,
            // The build page's (docs/web.md).
            Event::Fns { .. } => Change::Nothing,
        }
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

    /// The steps that took longest by `now_ns`, longest first: each step once, its times
    /// added up (the codegen of each crate is one), and only those that took a part of the
    /// build worth naming.
    pub fn slowest(&self, now_ns: u64) -> Vec<Took> {
        const LISTED: usize = 3;
        let mut by_step: Vec<Took> = Vec::new();
        for stepped in &self.steps {
            let took_ns = stepped.ended_ns.unwrap_or(now_ns).saturating_sub(stepped.began_ns);
            match by_step.iter_mut().find(|it| it.step == stepped.step) {
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

    /// The crates on the longest chain of what waits for what, by how long each was busy
    /// until `now_ns`, the one the others wait for last; and that time added up. Empty
    /// without a graph.
    pub fn critical_path(&self, now_ns: u64) -> (Vec<usize>, u64) {
        // A row's time is its last node's, so a crate of several nodes counts once.
        let mut last = vec![None; self.crates.len()];
        for (at, node) in self.graph.iter().enumerate() {
            last[node.row] = Some(at);
        }
        // The longest chain ending at each node, and the node before it there.
        let mut longest: Vec<(u64, Option<usize>)> = Vec::with_capacity(self.graph.len());
        for (at, node) in self.graph.iter().enumerate() {
            let own = match last[node.row] == Some(at) {
                true => self.crates[node.row].busy_ns(now_ns),
                false => 0,
            };
            let before = node
                .deps
                .iter()
                .filter_map(|&dep| longest.get(dep).map(|it| (it.0, dep)))
                .max_by_key(|it| it.0);
            longest.push((own + before.map_or(0, |it| it.0), before.map(|it| it.1)));
        }
        let Some(end) = (0..longest.len()).max_by_key(|&it| longest[it].0) else {
            return (Vec::new(), 0);
        };
        let mut chain: Vec<usize> = Vec::new();
        let mut at = Some(end);
        while let Some(node) = at {
            let row = self.graph[node].row;
            if chain.last() != Some(&row) {
                chain.push(row);
            }
            at = longest[node].1;
        }
        (chain, longest[end].0)
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
/// steps, and what the store had of its compile.
///
/// ```text
///         Took 2m15s: checking 14 files 1m40s, loading workspace 10.7s, compiling functions 4.20s
///        Cache cold: the store has none of this build
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
            .map(|it| {
                let what = it.of.as_deref().unwrap_or(it.step.whole());
                format!("{} {what} {}", it.step.word().to_lowercase(), took(it.took_ns))
            })
            .collect();
        lines.push(status("Took", &format!("{}: {}", took(took_ns), steps.join(", "))));
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
    lines
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
/// crate built on a terminal: cargo says each.
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
        Change::Began(at) if !live => {
            let stepped = &build.steps[*at];
            vec![status(stepped.step.word(), stepped.what())]
        }
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
