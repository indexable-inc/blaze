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
//! - [`api`]: the agent API's responses, for a client of it to print.
//!
//! A record costs what it changes: [`Build`] keeps indexes and counts as records come (no
//! scan of every crate, node or step), and the renderers write into one buffer each, with no
//! `String` per line.

#![forbid(unsafe_code)]

/// `write!` into a `String`, which cannot fail.
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {{
        let _ = ::std::fmt::Write::write_fmt(&mut *$out, format_args!($($arg)*));
    }};
}

pub mod api;
mod live;
mod plain;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use live::{FRAME_NS, Live};
pub use plain::Plain;

use std::fmt::{self, Write as _};
use std::hash::BuildHasher;

use blaze_hash::{FixedState, HashMap};
use blaze_ids::radix;
use blaze_protocol::events::{
    Cache, Cause, CrateState, Event, Fns, Level, NodeBatch, NodeKind, NodeState, Order, Record,
    SLOW_NS, Slow, Step, Summary, Took,
};
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;

/// The longest a plain rendering says nothing while a step is under way.
pub const HEARTBEAT_NS: u64 = 15_000_000_000;

/// No index: the end of an intrusive list, a kind of step with no count.
const NONE: u32 = u32::MAX;

/// A build as its records so far have it.
#[derive(Default, Debug)]
pub struct Build {
    pub command: Option<String>,
    /// Every step begun, in order.
    pub steps: Vec<Stepped>,
    /// The nodes of the build's graph ([`Event::Node`], [`Event::Nodes`]), each after what it
    /// waits for.
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
    /// Why each record that could not be read was refused, said as an error.
    pub malformed: Vec<String>,
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
    /// What each node of the graph waits for, by index in [`Build::graph`]: the node's
    /// [`Noded::deps`] is its range (CSR).
    deps: Vec<u32>,
    index: Index,
}

/// What [`Build::apply`] keeps besides what it shows, so that a record costs what it changes
/// and the region reads its counts whole. Each answer is what a scan of the build gives
/// (tests: `indexes_agree`).
#[derive(Default, Debug)]
struct Index {
    /// The row of [`Build::crates`] of each name, by its hash: 4 bytes a crate, one hash
    /// and one compare of names a lookup, the name kept once, in its row.
    rows: HashTable<u32>,
    /// The index in [`Build::graph`] of the node said last with each id.
    nodes: HashMap<u32, u32>,
    /// The steps under way, by index in [`Build::steps`], in the order they began.
    open: Vec<u32>,
    /// Each kind of step begun, in the order it first began ([`Build::strip_into`]).
    kinds: Vec<Kind>,
    /// The functions of every crate together.
    fns: Fns,
    /// How many crates have functions done and no total yet.
    unknown: u32,
    /// How many crates are in each state ([`slot`]), and how many have ended.
    states: [u32; STATES],
    ended: u32,
    /// Per node: whether it or something it waits for is under way, how many of what it
    /// waits for are, and whether anything waits for it.
    under_way: Vec<bool>,
    under: Vec<u32>,
    waited_for: Vec<bool>,
    /// Who waits for each node, as intrusive lists: the node's first edge, and each edge's
    /// node and next edge (8 bytes an edge).
    first_dependent: Vec<u32>,
    dependents: Vec<(u32, u32)>,
    /// The nodes of each row, as intrusive lists: the row's first, and each node's next.
    first_node: Vec<u32>,
    next_node: Vec<u32>,
    /// How many rows with a node of the graph are busy.
    busy_graphed: u32,
    /// The name of a node's row as it is looked up, and the nodes whose `under_way` may
    /// change: buffers kept for the next record.
    name: String,
    flips: Vec<u32>,
    /// The order of the nodes of an [`Event::Nodes`] as it is read, kept for the next.
    order: Order,
    /// Entries of [`Build::compiling`] a shorter list dropped, their names' buffers kept for
    /// a longer one.
    spare: Vec<(Slow, u64)>,
}

/// A kind of step begun: how many of it are under way, how long those done took, and the
/// step whose count it shows (the last one with a count), or [`NONE`]. 24 bytes.
#[derive(Debug, Clone, Copy)]
struct Kind {
    closed_ns: u64,
    open: u32,
    count: u32,
    step: Step,
}

const _: () = assert!(size_of::<Kind>() == 24);

/// A kind of step in the line of steps as [`Build::strip_into`] gives it, with the step
/// whose count it shows ([`NONE`] for none) and, for the renderer, how many characters
/// its text is. 24 bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StripRow {
    pub took_ns: u64,
    pub count: u32,
    pub chars: u32,
    pub step: Step,
    pub running: bool,
}

const _: () = assert!(size_of::<StripRow>() == 24);

const STATES: usize = 8;

/// The index of `state` in [`Index::states`].
fn slot(state: CrateState) -> usize {
    match state {
        CrateState::Waiting => 0,
        CrateState::Running => 1,
        CrateState::UpToDate => 2,
        CrateState::Built => 3,
        CrateState::Failed => 4,
        CrateState::Resolving => 5,
        CrateState::Resolved => 6,
        CrateState::Checking => 7,
    }
}

fn unknown(fns: Fns) -> bool {
    fns.total == 0 && fns.done > 0
}

impl Index {
    /// The row named `name`, a new one waiting from `at` if none is.
    fn row(&mut self, crates: &mut Vec<Crated>, name: &str, at: u64) -> usize {
        let hash = FixedState.hash_one(name);
        let entry = self.rows.entry(
            hash,
            |&row| crates[row as usize].name == name,
            |&row| FixedState.hash_one(crates[row as usize].name.as_str()),
        );
        match entry {
            Entry::Occupied(it) => *it.get() as usize,
            Entry::Vacant(it) => {
                let row = crates.len();
                it.insert(row as u32);
                crates.push(Crated::waiting(name, at));
                self.states[slot(CrateState::Waiting)] += 1;
                self.first_node.push(NONE);
                row
            }
        }
    }

    fn kind(&mut self, step: Step) -> &mut Kind {
        let at = match self.kinds.iter().position(|it| it.step == step) {
            Some(at) => at,
            None => {
                self.kinds.push(Kind { closed_ns: 0, open: 0, count: NONE, step });
                self.kinds.len() - 1
            }
        };
        &mut self.kinds[at]
    }

    /// Node `node` of row `row` was added, waiting for `deps`; its row is `busy` or not.
    fn node(&mut self, node: u32, row: usize, deps: &[u32], busy: bool) {
        let mut under = 0;
        for &dep in deps {
            let dep = dep as usize;
            self.waited_for[dep] = true;
            under += u32::from(self.under_way[dep]);
            let edge = self.dependents.len() as u32;
            self.dependents.push((node, self.first_dependent[dep]));
            self.first_dependent[dep] = edge;
        }
        self.under.push(under);
        self.under_way.push(busy || under > 0);
        self.waited_for.push(false);
        self.first_dependent.push(NONE);
        let first = self.first_node[row];
        if first == NONE && busy {
            self.busy_graphed += 1;
        }
        self.next_node.push(first);
        self.first_node[row] = node;
    }

    /// Row `row` became busy, or stopped being: each of its nodes, and what waits for them,
    /// may be under way now or no longer. Every change goes one way, so each node changes at
    /// most once.
    fn busy_changed(&mut self, row: usize, busy: bool, crates: &[Crated], graph: &[Noded]) {
        let mut node = self.first_node[row];
        if node == NONE {
            return;
        }
        if busy {
            self.busy_graphed += 1;
        } else {
            self.busy_graphed -= 1;
        }
        let mut flips = std::mem::take(&mut self.flips);
        while node != NONE {
            flips.push(node);
            node = self.next_node[node as usize];
        }
        while let Some(node) = flips.pop() {
            let at = node as usize;
            let now = crates[graph[at].row as usize].is_busy() || self.under[at] > 0;
            if now == self.under_way[at] {
                continue;
            }
            self.under_way[at] = now;
            let mut edge = self.first_dependent[at];
            while edge != NONE {
                let (dependent, next) = self.dependents[edge as usize];
                if now {
                    self.under[dependent as usize] += 1;
                } else {
                    self.under[dependent as usize] -= 1;
                }
                flips.push(dependent);
                edge = next;
            }
        }
        self.flips = flips;
    }
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

/// A kind of step in the line of steps ([`Build::strip_into`]): under way or done, how long all
/// of its kind took, and the count of the last.
#[derive(Debug, PartialEq, Eq)]
pub struct Strip {
    pub step: Step,
    pub running: bool,
    pub took_ns: u64,
    pub count: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Stepped {
    pub step: Step,
    pub of: Option<String>,
    pub began_ns: u64,
    pub ended_ns: Option<u64>,
    /// The last [`Event::Progress`] of it.
    pub progress: Option<Progress>,
}

#[derive(Debug, PartialEq, Eq)]
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
        // alloc: the CLI's form, once per ask; the renderers write `Count` in place
        Some(Count(progress, self.step).to_string())
    }
}

/// [`Stepped::count`] as it is written.
#[derive(Clone, Copy)]
struct Count<'a>(&'a Progress, Step);

impl fmt::Display for Count<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (progress, counts) = (self.0, self.1.counts());
        match progress.total {
            0 => write!(f, "{} {counts}", progress.done),
            total => write!(f, "{}/{total} {counts}", progress.done),
        }
    }
}

impl Count<'_> {
    /// How many characters it is written in.
    fn chars(self) -> usize {
        let total = match self.0.total {
            0 => 0,
            total => 1 + digits(total),
        };
        digits(self.0.done) + total + 1 + self.1.counts().len()
    }
}

fn digits(mut n: u64) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// A node of the build's graph: its id, the row of [`Build::crates`] it moves, and the range
/// of [`Build::deps`] it waits for. 20 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Noded {
    pub id: u32,
    pub row: u32,
    deps: (u32, u32),
    pub kind: NodeKind,
}

const _: () = assert!(size_of::<Noded>() == 20);

/// What a node of `kind` is a row of when that is not its crate's own: the words after the
/// crate's name.
fn own(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Names
        | NodeKind::Bodies
        | NodeKind::Codegen
        | NodeKind::Std
        | NodeKind::Manifest => None,
        NodeKind::Script | NodeKind::ScriptRun => Some("build script"),
        NodeKind::Macro => Some("proc macro"),
        NodeKind::Fetch => Some("source"),
        NodeKind::Store => Some("to the store"),
        NodeKind::Link => Some("link"),
        NodeKind::Test => Some("tests"),
    }
}

/// The word a row is under way with while its node of `kind` is.
fn doing(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Names | NodeKind::Bodies | NodeKind::Codegen | NodeKind::Std => None,
        NodeKind::Script => Some("compiling"),
        NodeKind::ScriptRun => Some("running"),
        NodeKind::Macro => Some("linking"),
        NodeKind::Fetch => Some("fetching"),
        NodeKind::Manifest => Some("reading"),
        NodeKind::Store => Some("writing"),
        NodeKind::Link => Some("linking"),
        NodeKind::Test => Some("running"),
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
        (_, NodeState::Failed { .. } | NodeState::Skipped { .. } | NodeState::Cancelled) => {
            CrateState::Failed
        }
    }
}

#[derive(Debug, PartialEq)]
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
        // alloc: once per row, its name kept for the build (the index hashes it, not a copy)
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

#[derive(Debug, PartialEq)]
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

/// What a record changed, for a renderer to say. What it says of the record's own text
/// borrows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change<'a> {
    Nothing,
    /// The step at this index of [`Build::steps`] began, or ended.
    Began(usize),
    Ended(usize),
    /// The crate at this index of [`Build::crates`] is in another state, or further.
    Crate {
        at: usize,
        moved: bool,
    },
    Said(Level, &'a str),
    Refused(usize),
    /// The record at this index of [`Build::malformed`] could not be read.
    Malformed(usize),
    Rebuilt(usize),
    Finished,
}

impl Build {
    pub fn apply<'a>(&mut self, record: &'a Record) -> Change<'a> {
        let at = record.at_ns;
        self.last_ns = self.last_ns.max(at);
        match &record.event {
            Event::Began { command, .. } => {
                // clone: once per build, the command it was asked for
                self.command = Some(command.clone());
                Change::Nothing
            }
            Event::Started { step, of } => {
                self.named(at);
                let index = self.steps.len();
                // clone: kept for the build, one per step begun
                let (of, began_ns, ended_ns, progress) = (of.clone(), at, None, None);
                self.steps.push(Stepped { step: *step, of, began_ns, ended_ns, progress });
                self.index.open.push(index as u32);
                self.index.kind(*step).open += 1;
                Change::Began(index)
            }
            Event::Progress { step, done, total, longest } => {
                let steps = &self.steps;
                let mut open = self.index.open.iter().rev();
                if let Some(&index) = open.find(|&&it| steps[it as usize].step == *step) {
                    match &mut self.steps[index as usize].progress {
                        Some(progress) => {
                            (progress.done, progress.total) = (*done, *total);
                            progress.longest.clone_from(longest);
                        }
                        none => {
                            // clone: once per step, its first progress; later ones reuse it
                            let longest = longest.clone();
                            *none = Some(Progress { done: *done, total: *total, longest });
                        }
                    }
                    let kind = self.index.kind(*step);
                    if kind.count == NONE || kind.count < index {
                        kind.count = index;
                    }
                }
                Change::Nothing
            }
            Event::Node { id, kind, name, deps } => {
                self.noded(*id, *kind, name, deps, at);
                Change::Nothing
            }
            Event::Nodes { first, kinds, names, name_at, deps, deps_at } => {
                let mut order = std::mem::take(&mut self.index.order);
                let read = NodeBatch::new(*first, kinds, names, name_at, deps, deps_at, &mut order);
                let change = match read {
                    Ok(batch) => {
                        for &place in order.places() {
                            let node = batch.node(place as usize);
                            self.noded(node.id, node.kind, node.name, node.deps, at);
                        }
                        Change::Nothing
                    }
                    Err(why) => {
                        // alloc: error path, a record that could not be read
                        self.malformed.push(why.to_string());
                        Change::Malformed(self.malformed.len() - 1)
                    }
                };
                self.index.order = order;
                change
            }
            Event::Moved { id, state } => match self.index.nodes.get(id) {
                Some(&node) => {
                    let Noded { kind, row, .. } = self.graph[node as usize];
                    let row = row as usize;
                    // A row of several nodes says what the one that moved last does.
                    self.crates[row].doing = doing(kind);
                    let moved = self.moved(row, crate_state(kind, state), at);
                    Change::Crate { at: row, moved }
                }
                None => Change::Nothing,
            },
            Event::Cache { cache } => {
                // clone: once per build
                self.cache = Some(cache.clone());
                Change::Nothing
            }
            Event::Ended { step, of } => {
                let steps = &self.steps;
                let open = self.index.open.iter().rposition(|&it| {
                    let it = &steps[it as usize];
                    it.step == *step && it.of == *of
                });
                match open {
                    Some(open) => {
                        let index = self.index.open.remove(open) as usize;
                        let stepped = &mut self.steps[index];
                        stepped.ended_ns = Some(at);
                        let took_ns = at.saturating_sub(stepped.began_ns);
                        let kind = self.index.kind(*step);
                        kind.open -= 1;
                        kind.closed_ns += took_ns;
                        if self.index.open.is_empty() {
                            self.idle = Some((at, *step));
                        }
                        Change::Ended(index)
                    }
                    None => Change::Nothing,
                }
            }
            Event::Crate { name, state, fns } => {
                let rows = self.crates.len();
                let index = self.index.row(&mut self.crates, name, at);
                let moved = self.moved(index, *state, at) || index == rows;
                let was = std::mem::replace(&mut self.crates[index].fns, *fns);
                let all = &mut self.index.fns;
                all.total = all.total - was.total + fns.total;
                all.done = all.done - was.done + fns.done;
                all.kept = all.kept - was.kept + fns.kept;
                self.index.unknown = self.index.unknown - u32::from(unknown(was))
                    + u32::from(unknown(*fns));
                Change::Crate { at: index, moved }
            }
            Event::Rebuilt { name, krate, cause } => {
                // clone: kept for the last line, one per function the daemon lists as rebuilt
                self.rebuilt.push((name.clone(), krate.clone(), cause.clone()));
                Change::Rebuilt(self.rebuilt.len() - 1)
            }
            Event::Unlisted { count } => {
                self.unlisted += count;
                Change::Nothing
            }
            Event::Refused { name, why } => {
                // clone: error path, a crate blaze does not build
                self.refused.push((name.clone(), why.clone()));
                Change::Refused(self.refused.len() - 1)
            }
            Event::Said { level, text } => Change::Said(*level, text),
            Event::Finished { ok, summary } => {
                self.named(at);
                // alloc: once per build, the rows that held it up
                let mut rows = Vec::new();
                let chain_ns = self.critical_path_into(at, &mut rows);
                // clone: once per build, the names of what held it up
                let chain = rows.iter().rev().map(|&it| self.crates[it].name.clone()).collect();
                // clone: once per build, the summary it ends with
                let (ok, summary, unnamed) = (*ok, summary.clone(), self.unnamed);
                // alloc: once per build
                let cores = summary.slowest.iter().map(|it| self.busy_in(it, at)).collect();
                let cores_of = self.cores_of;
                self.compiling.clear();
                // clone: once per build; `Build::slowest` stays as the records said it
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
                let nodes = &self.index.nodes;
                self.critical.clear();
                let critical = path.iter().filter_map(|it| nodes.get(it));
                self.critical.extend(critical.map(|&it| it as usize));
                Change::Nothing
            }
            Event::Cores { busy, of } => {
                self.cores.push((at, *busy));
                self.cores_of = *of;
                Change::Nothing
            }
            Event::Compiling { functions } => {
                let since = |it: &Slow| at.saturating_sub(it.ns);
                let spare = &mut self.index.spare;
                while self.compiling.len() > functions.len() {
                    spare.extend(self.compiling.pop());
                }
                let write = |kept: &mut (Slow, u64), it: &Slow| {
                    kept.0.krate.clone_from(&it.krate);
                    kept.0.name.clone_from(&it.name);
                    kept.0.ns = it.ns;
                    kept.1 = since(it);
                };
                for (kept, it) in self.compiling.iter_mut().zip(functions) {
                    write(kept, it);
                }
                for it in &functions[self.compiling.len()..] {
                    let entry = match spare.pop() {
                        Some(mut entry) => {
                            write(&mut entry, it);
                            entry
                        }
                        // clone: a list longer than any said before
                        None => (it.clone(), since(it)),
                    };
                    self.compiling.push(entry);
                }
                Change::Nothing
            }
            Event::Slowest { functions } => {
                self.slowest.clone_from(functions);
                Change::Nothing
            }
            // The build page's (https://github.com/indexable-inc/blaze/pull/1341).
            Event::Functions { .. } => Change::Nothing,
        }
    }

    /// Takes in node `id` of the graph, said at `at`: a row for it, and what it waits for of
    /// the nodes said before it.
    fn noded(&mut self, id: u32, kind: NodeKind, name: &str, deps: &[u32], at: u64) {
        let mut row_name = std::mem::take(&mut self.index.name);
        row_name.clear();
        row_name.push_str(name);
        if let Some(what) = own(kind) {
            row_name.push(' ');
            row_name.push_str(what);
        }
        let row = self.index.row(&mut self.crates, &row_name, at);
        self.index.name = row_name;
        let node = self.graph.len() as u32;
        let from = self.deps.len();
        let nodes = &self.index.nodes;
        self.deps.extend(deps.iter().filter_map(|dep| nodes.get(dep).copied()));
        let to = self.deps.len();
        let deps = (from as u32, to as u32);
        self.graph.push(Noded { id, row: row as u32, deps, kind });
        let busy = self.crates[row].is_busy();
        self.index.node(node, row, &self.deps[from..to], busy);
        self.index.nodes.insert(id, node);
    }

    /// Has the crate at `row` in `state` from `at` ([`Crated::moved`]), and the counts with it.
    fn moved(&mut self, row: usize, state: CrateState, at: u64) -> bool {
        let krate = &mut self.crates[row];
        let (was, was_ended) = (krate.state, krate.ended_ns.is_some());
        let moved = krate.moved(state, at);
        let ended = krate.ended_ns.is_some();
        let index = &mut self.index;
        index.states[slot(was)] -= 1;
        index.states[slot(state)] += 1;
        match (was_ended, ended) {
            (false, true) => index.ended += 1,
            (true, false) => index.ended -= 1,
            _ => {}
        }
        if busy(was) != busy(state) {
            index.busy_changed(row, busy(state), &self.crates, &self.graph);
        }
        moved
    }

    /// What node `node` of [`Build::graph`] waits for, by index in it.
    pub fn deps(&self, node: usize) -> &[u32] {
        let (from, to) = self.graph[node].deps;
        &self.deps[from as usize..to as usize]
    }

    /// How many crates are in `state`.
    pub fn in_state(&self, state: CrateState) -> usize {
        self.index.states[slot(state)] as usize
    }

    /// The functions of crate `krate` compiling for [`SLOW_NS`] or more by `now_ns`, longest
    /// first: their names and for how long. The scan the tests check `compiling_top` with.
    #[cfg(test)]
    pub fn compiling_of(&self, krate: &str, now_ns: u64) -> Vec<(&str, u64)> {
        let of = self.compiling.iter().filter(|(it, _)| it.krate == krate);
        let of = of.map(|(it, since)| (it.name.as_str(), now_ns.saturating_sub(*since)));
        let mut of: Vec<_> = of.filter(|it| it.1 >= SLOW_NS).collect();
        radix::sort_copy_by_key(&mut of, |it| !it.1);
        of
    }

    /// The functions of crate `krate` compiling for [`SLOW_NS`] or more by `now_ns`, longest
    /// first, the first `N` of them, each as its index in [`Build::compiling`], with no
    /// allocation, and how many there are of them.
    fn compiling_top<const N: usize>(
        &self,
        krate: &str,
        now_ns: u64,
    ) -> ([(usize, u64); N], usize) {
        let (mut top, mut len) = ([(0, 0); N], 0);
        for (at, (it, since)) in self.compiling.iter().enumerate() {
            let ns = now_ns.saturating_sub(*since);
            if ns < SLOW_NS || it.krate != krate {
                continue;
            }
            // After every one as long: the first said of the same length stays first.
            let place = top[..len].iter().position(|it| it.1 < ns).unwrap_or(len);
            if place == N {
                continue;
            }
            len = (len + 1).min(N);
            top.copy_within(place..len - 1, place + 1);
            top[place] = (at, ns);
        }
        (top, len)
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
    /// and how long they took by `now_ns`. Done ones too quick to matter are left out. The
    /// form the tests compare with the old renderer's; the region reads `strip_into`.
    #[cfg(test)]
    pub fn strip(&self, now_ns: u64) -> Vec<Strip> {
        let mut rows = Vec::new();
        self.strip_into(now_ns, &mut rows);
        let count = |at: u32| self.steps.get(at as usize).and_then(Stepped::count);
        let strip = rows.iter().map(|it| Strip {
            step: it.step,
            running: it.running,
            took_ns: it.took_ns,
            count: count(it.count),
        });
        strip.collect()
    }

    /// Each kind of step begun, in the order it first began, into `rows` (what [`Strip`]
    /// says, its count as the step that has it), from the kinds' totals: what a step of each
    /// kind under way took so far is all it adds up, not every step begun. Done ones too quick
    /// to matter are left out.
    pub(crate) fn strip_into(&self, now_ns: u64, rows: &mut Vec<StripRow>) {
        rows.clear();
        rows.extend(self.index.kinds.iter().map(|it| StripRow {
            took_ns: it.closed_ns,
            count: it.count,
            chars: 0,
            step: it.step,
            running: it.open > 0,
        }));
        for &open in &self.index.open {
            let stepped = &self.steps[open as usize];
            if let Some(row) = rows.iter_mut().find(|it| it.step == stepped.step) {
                row.took_ns += now_ns.saturating_sub(stepped.began_ns);
            }
        }
        rows.retain(|it| it.running || it.took_ns >= 50_000_000);
    }

    /// Every step in the order it began, and the time spent at no step between them, as
    /// `blaze why` lists them by `now_ns`: what it was, how long it took, and how many cores
    /// the worker kept busy meanwhile.
    pub fn why_steps(&self, now_ns: u64) -> Vec<String> {
        // Shorter steps say nothing of where the time went.
        const GAP_NS: u64 = 50_000_000;
        const STEP_NS: u64 = 10_000_000;
        let cores = |from: u64, to: u64| self.busy_between(from, to).map(|it| it.0);
        // alloc: `blaze why`, once per command: a row per step
        let mut rows: Vec<(String, u64, Option<f64>)> = Vec::new();
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
            // alloc: `blaze why`, once per command
            let mut what = Short(stepped.step, stepped.of.as_deref()).to_string();
            // `check 74 files` has its count already: the count's last word is what it is of.
            let of = stepped.of.as_deref().and_then(|it| it.rsplit(' ').next());
            if let Some(progress) = &stepped.progress
                && stepped.step.counts().rsplit(' ').next() != of
            {
                put!(&mut what, " {}", Count(progress, stepped.step));
            }
            rows.push((what, took_ns, cores(stepped.began_ns, ended)));
        }
        if !rows.is_empty() && now_ns.saturating_sub(free) >= GAP_NS {
            rows.push(("at no step".into(), now_ns - free, cores(free, now_ns)));
        }
        let width = rows.iter().map(|it| it.0.chars().count()).max().unwrap_or(0);
        let cores_of = self.cores_of;
        let line = |(what, ns, busy): &(String, u64, Option<f64>)| {
            // alloc: `blaze why`, once per command: a line per row
            let mut line = format!("{what:<width$}  {:>6}", took(*ns));
            if let Some(busy) = busy {
                put!(&mut line, "  {busy:.1}/{cores_of} cores");
            }
            line
        };
        // alloc: `blaze why`, once per command
        rows.iter().map(line).collect()
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

    /// The steps under way, the one begun last at the end.
    pub fn running(&self) -> impl Iterator<Item = &Stepped> {
        self.index.open.iter().map(|&it| &self.steps[it as usize])
    }

    /// What the records so far come to, for the last line of a build that took `took_ns`:
    /// the first few rebuilt, and a count of the rest.
    pub fn summary(&self, took_ns: u64, error: Option<String>) -> Summary {
        const LISTED: usize = 8;
        let rebuilt = self.rebuilt.iter().take(LISTED);
        // clone: once per build, the summary handed out
        let rebuilt = rebuilt.map(|(name, _, cause)| (name.clone(), cause.clone())).collect();
        let more = self.rebuilt.len().saturating_sub(LISTED) as u64 + self.unlisted;
        let crates = self.crates.len() - self.in_state(CrateState::Waiting);
        // clone: once per build, the summary handed out
        let (fns, cache, slowest) = (self.fns(), self.cache.clone(), self.slowest(took_ns));
        Summary { took_ns, crates: crates as u64, fns, rebuilt, more, error, cache, slowest }
    }

    /// The steps that took longest by `now_ns`, longest first: each step of the same thing
    /// once, its times added up (the codegen of every crate is one), and only those that took
    /// a part of the build worth naming.
    pub fn slowest(&self, now_ns: u64) -> Vec<Took> {
        const LISTED: usize = 3;
        // alloc: once per build, for its summary
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
                // clone: once per build, for its summary
                None => by_step.push(Took { step: stepped.step, of: stepped.of.clone(), took_ns }),
            }
        }
        // Longest first; of the same length, the one begun first.
        radix::sort_by_key(&mut by_step, |it| !it.took_ns);
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
        // alloc: once per `blaze why`; the region reuses its own (`critical_path_into`)
        let mut chain = Vec::new();
        let took_ns = self.critical_path_into(now_ns, &mut chain);
        (chain, took_ns)
    }

    /// [`Build::critical_path`], its rows written into `chain`.
    pub(crate) fn critical_path_into(&self, now_ns: u64, chain: &mut Vec<usize>) -> u64 {
        chain.clear();
        for &node in self.critical.iter().rev() {
            let row = self.graph[node].row as usize;
            if !chain.contains(&row) {
                chain.push(row);
            }
        }
        let (Some(&last), Some(&first)) = (chain.first(), chain.last()) else {
            return 0;
        };
        let last = &self.crates[last];
        let ended = match (last.ended_ns, last.is_busy()) {
            (Some(ended), _) => ended,
            (None, true) => now_ns,
            // Resolved and waiting to be compiled: done with what it did last.
            (None, false) => last.since_ns,
        };
        ended.saturating_sub(self.crates[first].began_ns)
    }

    /// The functions of every crate together.
    pub fn fns(&self) -> Fns {
        self.index.fns
    }
}

/// The last `most` characters of `text`, after an ellipsis when it is longer: a function's
/// path says which it is at its end.
pub fn tail(text: &str, most: usize) -> String {
    // alloc: the CLI's form; the last lines write [`Tail`] in place
    Tail(text, most).to_string()
}

/// [`tail`] as it is written.
#[derive(Clone, Copy)]
struct Tail<'a>(&'a str, usize);

impl fmt::Display for Tail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (text, most) = (self.0, self.1);
        let count = text.chars().count();
        if count <= most {
            return f.write_str(text);
        }
        f.write_char('\u{2026}')?;
        text.chars().skip(count + 1 - most).try_for_each(|it| f.write_char(it))
    }
}

/// A duration as a person reads it: `0.21s`, `33.2s`, `1m17s`.
pub fn took(ns: u64) -> String {
    // alloc: the CLI's form, padded by its callers; the renderers write [`Dur`] in place
    Dur(ns).to_string()
}

/// [`took`] as it is written. It takes no width (`{:>6}`): pad [`took`]'s `String`.
#[derive(Clone, Copy)]
struct Dur(u64);

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0 / 1_000_000;
        match ms {
            0..=9_999 => write!(f, "{}.{:02}s", ms / 1000, ms % 1000 / 10),
            10_000..=59_999 => write!(f, "{}.{}s", ms / 1000, ms % 1000 / 100),
            _ => write!(f, "{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
        }
    }
}

impl Dur {
    /// How many characters it is written in: `0.21s` and `33.2s` are five.
    fn chars(self) -> usize {
        match self.0 / 1_000_000 {
            0..=59_999 => 5,
            ms => digits(ms / 60_000) + 4,
        }
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
        // alloc: none, an empty `Vec` holds no buffer
        return Vec::new();
    }
    let mut lines = why_lines(finished, color);
    lines.insert(0, summary_line(finished, color));
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
    // alloc: once per build, its last lines
    let mut lines = Vec::new();
    if took_ns < WHY_NS || !finished.ok {
        return lines;
    }
    // alloc: once per build, a last line
    let new_line = String::new;
    let started = |word: &str| {
        let mut line = new_line();
        status(&mut line, word, color);
        line
    };
    if !summary.slowest.is_empty() {
        let mut line = started("Took");
        put!(&mut line, "{}: ", Dur(took_ns));
        let of = finished.cores_of;
        for (at, it) in summary.slowest.iter().enumerate() {
            if at > 0 {
                line.push_str(", ");
            }
            put!(&mut line, "{} {}", Short(it.step, it.of.as_deref()), Dur(it.took_ns));
            match finished.cores.get(at).copied().flatten() {
                Some(busy) if at == 0 => put!(&mut line, " ({busy:.1}/{of} cores)"),
                Some(busy) => put!(&mut line, " ({busy:.1}/{of})"),
                None => {}
            }
        }
        lines.push(line);
    }
    if !finished.slowest.is_empty() {
        const LISTED: usize = 5;
        let mut line = started("Slowest");
        line.push_str("fns: ");
        for (at, it) in finished.slowest.iter().take(LISTED).enumerate() {
            if at > 0 {
                line.push_str(", ");
            }
            put!(&mut line, "{} {}", Tail(&it.name, 48), Dur(it.ns));
        }
        match finished.slowest.len().saturating_sub(LISTED) {
            0 => {}
            more => put!(&mut line, " (and {more} more)"),
        }
        lines.push(line);
    }
    // The line before says what was compiled and kept; this says what it does not.
    if let Some(cache) = &summary.cache {
        match &cache.cold {
            Some(why) => {
                let mut line = started("Cache");
                put!(&mut line, "cold: {why}");
                lines.push(line);
            }
            None if cache.fetched > 0 => {
                let mut line = started("Cache");
                let (found, fetched) = (cache.found, cache.fetched);
                put!(&mut line, "{found} found here, {fetched} fetched from the remote store");
                lines.push(line);
            }
            None => {}
        }
    }
    if finished.chain.len() > 1 && finished.chain_ns > 0 {
        let mut line = started("Chain");
        for (at, name) in finished.chain.iter().enumerate() {
            if at > 0 {
                line.push_str(" \u{2192} ");
            }
            line.push_str(name);
        }
        put!(&mut line, " {}", Dur(finished.chain_ns));
        lines.push(line);
    }
    if let Some(unnamed) = finished.unnamed.filter(|it| it.took_ns >= UNNAMED_NS) {
        let (mut line, after, took) = (new_line(), unnamed.after.whole(), Dur(unnamed.took_ns));
        let warning = Level::Warning;
        said(&mut line, warning, format_args!("{took} with no step named, after {after}"), color);
        lines.push(line);
    }
    if !lines.is_empty() {
        let mut line = new_line();
        put!(
            &mut line,
            "{}",
            Painted(DIM, format_args!("{:>12} `blaze why` says more", ""), color)
        );
        lines.push(line);
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
#[derive(Clone, Copy)]
struct Short<'a>(Step, Option<&'a str>);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = label(self.0);
        match self.1 {
            None => f.write_str(label),
            Some(of) => {
                let of = of.strip_prefix("the standard library ").unwrap_or(of);
                write!(f, "{label} {of}")
            }
        }
    }
}

/// The last line of a build: what it rebuilt and why, or why it failed, and how long it took.
/// Not cargo's `Finished`, which `blaze` says after it.
pub fn summary_line(finished: &Finished, color: bool) -> String {
    let summary = &finished.summary;
    let took = Dur(took_ns(finished));
    // alloc: once per build, its last line
    let mut line = String::new();
    if !finished.ok {
        let why = summary.error.as_deref().unwrap_or("the build failed");
        put!(&mut line, "{}: {why} (after {took})", Painted(RED, "error", color));
        return line;
    }
    let compiled = summary.fns.compiled();
    if !summary.rebuilt.is_empty() {
        status(&mut line, "Rebuilt", color);
        let listed = summary.rebuilt.len() as u64 + summary.more;
        put!(&mut line, "{listed} fn{}: ", plural_s(listed));
        causes(&mut line, &summary.rebuilt);
        if summary.more > 0 {
            put!(&mut line, " and {} more", summary.more);
        }
    } else if compiled > 0 {
        status(&mut line, "Compiled", color);
        let (kept, crates) = (summary.fns.kept, summary.crates);
        let (fns, crates_s) = (plural_s(compiled), plural_s(crates));
        put!(&mut line, "{compiled} fn{fns} in {crates} crate{crates_s}, {kept} kept");
    } else if let Some(cache) = summary.cache.as_ref().filter(|it| it.found + it.fetched > 0) {
        status(&mut line, "Built", color);
        let found = cache.found + cache.fetched;
        let (fns, compiled) = (plural_s(found), cache.compiled);
        put!(&mut line, "from the store: {found} fn{fns} found, {compiled} compiled");
    } else {
        status(&mut line, "Fresh", color);
        line.push_str("nothing to rebuild");
    }
    put!(&mut line, "; {took}");
    line
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

/// Writes the lines `change` adds that stay, each ending in a newline: on a terminal
/// (`live`) above the region, elsewhere all there is.
fn staying(
    build: &Build,
    change: &Change<'_>,
    color: bool,
    live: bool,
    errors_said: bool,
    out: &mut String,
) {
    match *change {
        Change::Ended(at) => {
            let stepped = &build.steps[at];
            let took_ns = stepped.ended_ns.unwrap_or(stepped.began_ns) - stepped.began_ns;
            // The crates say how their compiling went.
            let said_by_crates = stepped.step == Step::Codegen && !build.crates.is_empty();
            if took_ns >= NEWS_NS && !said_by_crates {
                status(out, done_word(stepped.step), color);
                put!(out, "{} in {}\n", stepped.what(), Dur(took_ns));
            }
        }
        Change::Crate { at, moved: true } => {
            let krate = &build.crates[at];
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
                    status(out, "Compiling", color);
                    out.push_str(&krate.name);
                    out.push('\n');
                }
                // Of a graph's crates the tree showed each, and `blaze why` lists them.
                CrateState::Built if build.graph.is_empty() && (live || took_ns >= NEWS_NS) => {
                    status(out, "Compiled", color);
                    out.push_str(&krate.name);
                    if krate.fns.total != 0 {
                        let compiled = krate.fns.compiled();
                        let (s, kept) = (plural_s(compiled), krate.fns.kept);
                        put!(out, ": {compiled} fn{s} compiled, {kept} kept");
                    }
                    put!(out, " in {}\n", Dur(took_ns));
                }
                _ => {}
            }
        }
        Change::Said(level, text) => {
            said(out, level, text, color);
            out.push('\n');
        }
        Change::Refused(at) => {
            let (name, why) = &build.refused[at];
            said(out, Level::Error, format_args!("blaze does not build `{name}`: {why}"), color);
            out.push('\n');
        }
        Change::Malformed(at) => {
            said(out, Level::Error, &build.malformed[at], color);
            out.push('\n');
        }
        Change::Finished if !live => {
            for finished in &build.finished {
                for line in last_lines(finished, color, errors_said) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

/// `word` right-aligned in 12 columns and painted, then a space: what cargo's status lines
/// start with.
fn status(out: &mut String, word: &str, color: bool) {
    put!(out, "{} ", Painted(GREEN, format_args!("{word:>12}"), color));
}

/// `render (body edited), draw, tick (calls render)`, written into `out`: names with the same
/// cause one after another say it once, after the last of them.
fn causes(out: &mut String, rebuilt: &[(String, Cause)]) {
    for (at, (name, cause)) in rebuilt.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(name);
        if rebuilt.get(at + 1).is_none_or(|next| next.1 != *cause) {
            put!(out, " ({})", cause.short());
        }
    }
}

/// `word`, plural but for one: the old renderer's (`tests/old.rs`); this one writes
/// [`plural_s`].
#[cfg(test)]
fn plural(n: u64, word: &str) -> String {
    if n == 1 { word.to_owned() } else { format!("{word}s") }
}

fn plural_s(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

const GREEN: &str = "\x1b[1m\x1b[92m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// `T` written in `color` when `colored`, then the reset. It takes no width: pad what it
/// paints.
struct Painted<T>(&'static str, T, bool);

impl<T: fmt::Display> fmt::Display for Painted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.2 {
            true => write!(f, "{}{}{RESET}", self.0, self.1),
            false => write!(f, "{}", self.1),
        }
    }
}

/// A line of `level`, as cargo writes them, written into `out` with no newline.
fn said(out: &mut String, level: Level, text: impl fmt::Display, color: bool) {
    let (word, color_of) = match level {
        Level::Note => ("note", GREEN),
        Level::Warning => ("warning", YELLOW),
        Level::Error => ("error", RED),
    };
    put!(out, "{}: {text}", Painted(color_of, word, color));
}
