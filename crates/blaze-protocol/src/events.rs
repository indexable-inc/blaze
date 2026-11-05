//! What a build says as it goes, for whoever watches it: `blaze build` and `run` render it
//! live, `blaze status` and `watch` attach to a build another terminal started, `--events`
//! writes it for tools, and `blaze why` reads the last build's. The daemon keeps each
//! build's records in order; the same build gives the same records but for their times.
//!
//! Times are nanoseconds since the build began and counts are integers: no floats.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt::{self, Write as _};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One thing a build said, `at_ns` after it began.
// clone: the edge; the daemon hands a build's records to each subscriber, and the plane's and
// UI's models keep the ones they are told.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub at_ns: u64,
    pub event: Event,
}

crate::in_key_order! {
// clone: as `Record`, which holds it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A build began: the command that asked for it, in the workspace at `root`.
    Began { build: u64, command: String, root: PathBuf } keys [root, build, command],
    /// A step of the build began, of what when it is of one crate or program.
    Started { step: Step, of: Option<String> } keys [of, step],
    /// The step began last ended.
    Ended { step: Step, of: Option<String> } keys [of, step],
    /// How far the step under way is: `done` of `total` of what it counts ([`Step::counts`]),
    /// `total` 0 while it is not known, and what of it has taken longest so far.
    Progress { step: Step, done: u64, total: u64, longest: Option<String> }
        keys [done, step, total, longest],
    /// How far the build of a crate is.
    Crate { name: String, state: CrateState, fns: Fns } keys [fns, name, state],
    /// Why a unit or a function was compiled again rather than taken as it was.
    Rebuilt { name: String, krate: String, cause: Cause } keys [name, cause, krate],
    /// How many more functions were compiled again than the [`Event::Rebuilt`] records name.
    Unlisted { count: u64 } keys [count],
    /// What the store had of the build's compile, and when it had little, why.
    Cache { cache: Cache } keys [cache],
    /// What blaze does not build, which fails the build (docs/architecture.md, "Never
    /// guess; refuse visibly").
    Refused { name: String, why: String } keys [why, name],
    /// A line that is no step: a warning, a note.
    Said { level: Level, text: String } keys [text, level],
    /// The build is done.
    Finished { ok: bool, summary: Summary } keys [ok, summary],
    /// A node of the build's graph (docs/driver.md), said once, before it first moves: what it
    /// is and the nodes it waits for.
    Node { id: u32, kind: NodeKind, name: String, deps: Vec<u32> } keys [id, deps, kind, name],
    /// Nodes of the graph said together, each as its [`Event::Node`] would say it, before any
    /// of them first moves: node `i` of the `kinds.len()` is `first + i`, of kind `kinds[i]`,
    /// named `names[name_at[i]..name_at[i + 1]]` (in bytes) and waiting for
    /// `deps[deps_at[i]..deps_at[i + 1]]`. `name_at` and `deps_at` each hold `kinds.len() + 1`
    /// offsets, from 0 to the length of `names` and of `deps`, never going back, and `name_at`
    /// cuts no character. A dep is a node said before or another of these, given before or
    /// after it here; none of these waits for itself through the others. A reader takes it
    /// through [`NodeBatch::new`], which refuses one that is not so.
    Nodes {
        first: u32,
        kinds: Vec<NodeKind>,
        names: String,
        name_at: Vec<u32>,
        deps: Vec<u32>,
        deps_at: Vec<u32>,
    } keys [deps, first, kinds, names, deps_at, name_at],
    /// A node of the graph is now in `state`.
    Moved { id: u32, state: NodeState } keys [id, state],
    /// What held the build up: each node of it is the one the next waited for last, from the
    /// first to the one that finished last.
    Critical { path: Vec<u32> } keys [path],
    /// How busy the process was since it said this last, while at a step: `busy` hundredths
    /// of a core, of the `of` cores it may run on. Said when it moved by a tenth of a core.
    Cores { busy: u32, of: u32 } keys [of, busy],
    /// The functions compiling for longer than a moment ([`SLOW_NS`]), longest first, each
    /// for `ns` by this record: said when which they are changes, and empty once none is.
    Compiling { functions: Vec<Slow> } keys [functions],
    /// The functions the build took [`SLOW_NS`] or more to compile, longest first, each in
    /// `ns`: said once, as it ends.
    Slowest { functions: Vec<Slow> } keys [functions],
    /// Functions put in object `object` of the process's builds, as they were put in it. An
    /// object is `whole`, all of the program, or a patch of the last whole one with what an
    /// edit compiled again. A function is in one record of an object. The daemon sends these
    /// to no command: a cold build's are some megabytes.
    Functions { object: u32, whole: bool, functions: Vec<Function> }
        keys [whole, object, functions],
}
}

impl Event {
    /// The nodes of an [`Event::Nodes`], checked by [`NodeBatch::new`] with `order`; `None` for
    /// another event.
    pub fn nodes<'a>(&'a self, order: &mut Order) -> Option<Result<NodeBatch<'a>, Malformed>> {
        match self {
            Event::Nodes { first, kinds, names, name_at, deps, deps_at } => {
                Some(NodeBatch::new(*first, kinds, names, name_at, deps, deps_at, order))
            }
            _ => None,
        }
    }
}

/// An [`Event::Nodes`] whose columns agree, so that each node reads without a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeBatch<'a> {
    first: u32,
    kinds: &'a [NodeKind],
    names: &'a str,
    name_at: &'a [u32],
    deps: &'a [u32],
    deps_at: &'a [u32],
}

/// A node of a [`NodeBatch`]: what its [`Event::Node`] would say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeOf<'a> {
    pub id: u32,
    pub kind: NodeKind,
    pub name: &'a str,
    pub deps: &'a [u32],
}

impl NodeOf<'_> {
    pub fn event(&self) -> Event {
        // alloc: an owned event handed out, for a reader that wants one; a batch reads in place.
        let (name, deps) = (self.name.to_owned(), self.deps.to_vec());
        Event::Node { id: self.id, kind: self.kind, name, deps }
    }
}

/// Why an [`Event::Nodes`] cannot be read: its columns disagree, or its nodes wait for each
/// other in a cycle.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed(pub String);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a record of nodes is malformed: {}", self.0)
    }
}

impl std::error::Error for Malformed {}

/// What reads [`Event::Nodes`] records keeps from one to the next: the order of the last one
/// read ([`Order::places`]) and the buffers that found it.
#[derive(Debug, Default)]
pub struct Order {
    places: Vec<u32>,
    waiting: Vec<u32>,
    first_dependent: Vec<u32>,
    /// Each edge's dependent and the next edge of the same dep.
    dependents: Vec<(u32, u32)>,
    ready: BinaryHeap<Reverse<u32>>,
}

const NO_EDGE: u32 = u32::MAX;

impl Order {
    /// The places of the last [`NodeBatch`] read with it, each after the others of the batch it
    /// waits for, the first given first among those that can go: the order their
    /// [`Event::Node`]s would come in.
    pub fn places(&self) -> &[u32] {
        &self.places
    }
}

/// `at` holds offsets for `len` nodes into `end` entries: `len + 1` of them, from 0 to `end`,
/// never going back.
fn offsets(column: &str, at: &[u32], len: usize, end: usize) -> Result<(), Malformed> {
    if at.len() != len + 1 || at[0] != 0 || at[len] as usize != end {
        let offsets = at.len();
        // alloc: error path, a malformed record.
        let why = format!("{column} of {offsets} offsets for {len} nodes and {end} entries");
        return Err(Malformed(why));
    }
    match at.windows(2).position(|pair| pair[0] > pair[1]) {
        // alloc: error path, a malformed record.
        Some(i) => Err(Malformed(format!("{column} goes back at node {i}"))),
        None => Ok(()),
    }
}

impl<'a> NodeBatch<'a> {
    /// The columns of an [`Event::Nodes`], checked: refused unless they agree as the event
    /// says. `order` is left with the order of their nodes ([`Order::places`]), or empty.
    pub fn new(
        first: u32,
        kinds: &'a [NodeKind],
        names: &'a str,
        name_at: &'a [u32],
        deps: &'a [u32],
        deps_at: &'a [u32],
        order: &mut Order,
    ) -> Result<NodeBatch<'a>, Malformed> {
        order.places.clear();
        let len = kinds.len();
        if u32::try_from(len).ok().and_then(|it| first.checked_add(it)).is_none() {
            // alloc: error path, a malformed record.
            return Err(Malformed(format!("{len} ids from {first} past u32")));
        }
        offsets("name_at", name_at, len, names.len())?;
        offsets("deps_at", deps_at, len, deps.len())?;
        if let Some(i) = name_at.iter().position(|&at| !names.is_char_boundary(at as usize)) {
            // alloc: error path, a malformed record.
            return Err(Malformed(format!("name_at cuts a character at node {i}")));
        }
        let batch = NodeBatch { first, kinds, names, name_at, deps, deps_at };
        batch.order(order)?;
        Ok(batch)
    }

    pub fn first(&self) -> u32 {
        self.first
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// The node at `at`, below [`NodeBatch::len`].
    pub fn node(&self, at: usize) -> NodeOf<'a> {
        let names = self.names;
        NodeOf {
            id: self.first + at as u32,
            kind: self.kinds[at],
            name: &names[self.name_at[at] as usize..self.name_at[at + 1] as usize],
            deps: self.deps_of(at),
        }
    }

    /// Its nodes in the order given, the ids from [`NodeBatch::first`] on.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = NodeOf<'a>> + 'a {
        let batch = *self;
        (0..batch.len()).map(move |at| batch.node(at))
    }

    fn deps_of(&self, at: usize) -> &'a [u32] {
        let deps = self.deps;
        &deps[self.deps_at[at] as usize..self.deps_at[at + 1] as usize]
    }

    /// The place of `dep` in the batch, if it is one of it.
    fn place(&self, dep: u32) -> Option<u32> {
        let at = dep.wrapping_sub(self.first);
        (at < self.kinds.len() as u32).then_some(at)
    }

    /// Puts its places in `order`, each after those it waits for, the first given first among
    /// those that can go (Kahn's, by a heap): as given when each comes after its deps of the
    /// batch, which costs one pass. Refuses a cycle.
    fn order(&self, order: &mut Order) -> Result<(), Malformed> {
        let len = self.len();
        let Order { places, waiting, first_dependent, dependents, ready } = order;
        places.clear();
        let in_order = (0..len).all(|at| {
            self.deps_of(at).iter().all(|&dep| self.place(dep).is_none_or(|it| (it as usize) < at))
        });
        if in_order {
            places.extend(0..len as u32);
            return Ok(());
        }
        waiting.clear();
        waiting.resize(len, 0);
        first_dependent.clear();
        first_dependent.resize(len, NO_EDGE);
        dependents.clear();
        ready.clear();
        for at in 0..len {
            for &dep in self.deps_of(at) {
                if let Some(it) = self.place(dep) {
                    waiting[at] += 1;
                    let edge = dependents.len() as u32;
                    dependents.push((at as u32, first_dependent[it as usize]));
                    first_dependent[it as usize] = edge;
                }
            }
        }
        ready.extend((0..len as u32).filter(|&at| waiting[at as usize] == 0).map(Reverse));
        while let Some(Reverse(at)) = ready.pop() {
            places.push(at);
            let mut edge = first_dependent[at as usize];
            while edge != NO_EDGE {
                let (dependent, next) = dependents[edge as usize];
                waiting[dependent as usize] -= 1;
                if waiting[dependent as usize] == 0 {
                    ready.push(Reverse(dependent));
                }
                edge = next;
            }
        }
        if places.len() == len {
            return Ok(());
        }
        places.clear();
        Err(self.cycle(waiting))
    }

    /// Why the nodes still `waiting` cannot go: they wait for each other.
    #[cold]
    fn cycle(&self, waiting: &[u32]) -> Malformed {
        const SHOWN: usize = 8;
        let mut stuck =
            (0..self.len()).filter(|&at| waiting[at] > 0).map(|at| self.first + at as u32);
        // alloc: error path, a record of nodes that wait for each other.
        let mut why = String::from("nodes ");
        for (i, id) in stuck.by_ref().take(SHOWN).enumerate() {
            if i > 0 {
                why.push_str(", ");
            }
            let _ = write!(why, "{id}");
        }
        let more = stuck.count();
        if more > 0 {
            let _ = write!(why, " and {more} more");
        }
        why.push_str(" wait for each other");
        Malformed(why)
    }
}

/// A function compiling longer than this is a slow one: [`Event::Compiling`] says it.
pub const SLOW_NS: u64 = 200_000_000;

/// A function slow to compile, of crate `krate`: `ns` is how long it took, or has so far.
crate::in_key_order! {
// clone: as `Record`, whose `Event::Compiling` holds it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Slow {
    pub krate: String,
    pub name: String,
    pub ns: u64,
}
keys [ns, name, krate]
}

crate::in_key_order! {
/// A function or instance of the program a build writes: what the build page draws.
// clone: as `Record`, whose `Event::Functions` holds it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// Its number in its object, which the `calls` of the object's others name it by.
    pub id: u32,
    /// Which function it is, the same in every build of the process that writes it: the
    /// functions of an edit are matched by it to those of the build before.
    pub key: u64,
    /// The crate it is written in, or for code of no function (drop glue, a shim) the one
    /// it is compiled for.
    pub krate: String,
    /// How a backtrace names it: its path, `<T as Trait>::method`, `{closure#0}`.
    pub name: String,
    /// The functions of the object it calls, by `id`. One it calls that is in no record of
    /// the object is code of another (rustc's, the base an edit patches).
    pub calls: Vec<u32>,
    pub how: How,
}
keys [id, how, key, name, calls, krate]
}

/// Where the code of a [`Function`] came from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum How {
    /// Compiled by this build, in `ns` of a thread's time.
    Compiled { ns: u64 },
    /// Compiled by this build with the others of its unit, by LLVM: no time of its own.
    Unit,
    /// What an earlier build of the process compiled, unchanged since.
    Unchanged,
    /// Taken from the store.
    Stored,
}

/// What a node of the build's graph does (docs/driver.md, "The graph").
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    /// Reading the manifests and the lock file.
    Manifest,
    /// A package's source.
    Fetch,
    /// Compiling a build script.
    Script,
    /// Running a build script.
    ScriptRun,
    /// A proc macro's library, built and loaded.
    Macro,
    /// The standard library.
    Std,
    /// A crate's names (its def map).
    Names,
    /// A crate's bodies: inference, borrow checking, MIR.
    Bodies,
    /// A unit of functions compiled or taken from the store.
    Codegen,
    /// Writing compiled functions to the store.
    Store,
    /// Linking a program.
    Link,
    /// Running a test executable.
    Test,
}

impl NodeKind {
    /// Every kind, in declaration order: `ALL[kind as usize] == kind`.
    pub const ALL: [NodeKind; 12] = [
        NodeKind::Manifest,
        NodeKind::Fetch,
        NodeKind::Script,
        NodeKind::ScriptRun,
        NodeKind::Macro,
        NodeKind::Std,
        NodeKind::Names,
        NodeKind::Bodies,
        NodeKind::Codegen,
        NodeKind::Store,
        NodeKind::Link,
        NodeKind::Test,
    ];

    /// The word a `timeline-us:` line names it by.
    pub const fn name(self) -> &'static str {
        match self {
            NodeKind::Manifest => "manifest",
            NodeKind::Fetch => "fetch",
            NodeKind::Script => "script",
            NodeKind::ScriptRun => "script-run",
            NodeKind::Macro => "macro",
            NodeKind::Std => "std",
            NodeKind::Names => "names",
            NodeKind::Bodies => "bodies",
            NodeKind::Codegen => "codegen",
            NodeKind::Store => "store",
            NodeKind::Link => "link",
            NodeKind::Test => "test",
        }
    }
}

const _: () = {
    let mut at = 0;
    while at < NodeKind::ALL.len() {
        assert!(NodeKind::ALL[at] as usize == at);
        at += 1;
    }
};

/// Where a node of the build's graph is. A node only moves down this list, from `Waiting`
/// to one of the last five.
// clone: the plane hands each subscriber the state a node moved to.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeState {
    /// A node it waits for is not done.
    Waiting,
    /// Everything it waits for is done; it waits for a thread.
    Ready,
    Running,
    /// Done, by doing its work.
    Built,
    /// Done, by taking what the store had: this machine's, or the remote's.
    Cached {
        remote: bool,
    },
    Failed {
        why: String,
    },
    /// Never run: `because` failed, and it waited for it, or the build stopped there.
    Skipped {
        because: u32,
    },
    /// Never run: every request it was part of was cancelled before it started.
    Cancelled,
}

impl NodeState {
    /// Whether the node is done, well or not: it moves no more.
    pub fn settled(&self) -> bool {
        matches!(
            self,
            NodeState::Built
                | NodeState::Cached { .. }
                | NodeState::Failed { .. }
                | NodeState::Skipped { .. }
                | NodeState::Cancelled
        )
    }
}

/// The steps of a build, in the order they come.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// Reading the manifests and the lock file, running build scripts and proc macros:
    /// rust-analyzer's database is made.
    LoadWorkspace,
    /// Fetching what the build needs that is not on the machine: crates, a toolchain.
    Fetch,
    /// Resolving the names of the crates.
    Resolve,
    /// Type checking, borrow checking and lowering to MIR.
    FrontEnd,
    /// Compiling functions to machine code, or taking them from the store.
    Codegen,
    /// The standard library, compiled once and kept in the store.
    Std,
    /// Writing the program.
    Link,
    /// `blaze check`'s diagnostics.
    Check,
    /// Running the program (`blaze run`).
    Run,
    /// Running tests (`blaze test`).
    Test,
    /// Compiling and running the build scripts of the packages, and linking their proc macros.
    BuildScripts,
    /// Loading the procedural macros, each compiled to WebAssembly once.
    Macros,
    /// Optimizing the compiled code (`--release`).
    Optimize,
    /// Writing what the build compiled to the store, for the next build.
    Store,
    /// Fetching what other machines compiled from the remote store, or sending it there.
    Remote,
}

impl Step {
    /// The word a line says it with, cargo's where cargo has one.
    pub fn word(self) -> &'static str {
        match self {
            Step::LoadWorkspace => "Loading",
            Step::Fetch => "Fetching",
            Step::Resolve => "Resolving",
            Step::FrontEnd => "Checking",
            Step::Codegen => "Compiling",
            Step::Std => "Compiling",
            Step::Link => "Linking",
            Step::Check => "Checking",
            Step::Run => "Running",
            Step::Test => "Testing",
            Step::BuildScripts => "Building",
            Step::Macros => "Loading",
            Step::Optimize => "Optimizing",
            Step::Store => "Storing",
            Step::Remote => "Fetching",
        }
    }

    /// What it is of when it is of no one crate.
    pub fn whole(self) -> &'static str {
        match self {
            Step::LoadWorkspace => "workspace",
            Step::Fetch => "dependencies",
            Step::Resolve => "names",
            Step::FrontEnd => "types",
            Step::Codegen => "functions",
            Step::Std => "the standard library (once, kept in the store)",
            Step::Link => "program",
            Step::Check => "crates",
            Step::Run => "program",
            Step::Test => "tests",
            Step::BuildScripts => "build scripts and proc macros",
            Step::Macros => "proc macros",
            Step::Optimize => "code",
            Step::Store => "the build in the store",
            Step::Remote => "from the remote store",
        }
    }

    /// What a [`Event::Progress`] of the step counts.
    pub fn counts(self) -> &'static str {
        match self {
            Step::LoadWorkspace | Step::Resolve | Step::Std => "crates",
            Step::Fetch => "packages",
            Step::FrontEnd => "bodies",
            Step::Codegen | Step::Optimize => "fns",
            Step::Link => "objects",
            Step::Check => "files",
            Step::Run => "programs",
            Step::Test => "tests",
            Step::BuildScripts => "steps",
            Step::Macros => "macros",
            Step::Store | Step::Remote => "entries",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrateState {
    Waiting,
    /// Its functions are being compiled.
    Running,
    /// Every function was in the store or unchanged.
    UpToDate,
    Built,
    Failed,
    /// Its names are being resolved: its modules read, its macros expanded.
    Resolving,
    /// Its names are resolved, and it waits for its functions to be compiled.
    Resolved,
    /// Its bodies are being checked: types, borrows, lints.
    Checking,
}

impl CrateState {
    /// Whether nothing more is done of the crate in this build.
    pub fn is_done(self) -> bool {
        matches!(self, CrateState::UpToDate | CrateState::Built | CrateState::Failed)
    }

    /// What a crate in the state is said to be doing, in a word.
    pub fn word(self) -> &'static str {
        match self {
            CrateState::Waiting => "waiting",
            CrateState::Running => "compiling",
            CrateState::UpToDate => "up to date",
            CrateState::Built => "built",
            CrateState::Failed => "failed",
            CrateState::Resolving => "resolving",
            CrateState::Resolved => "resolved",
            CrateState::Checking => "checking",
        }
    }
}

crate::in_key_order! {
/// The functions of a crate: how many the build has, how many are done, and of those how
/// many came from the store or were unchanged rather than compiled.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fns {
    pub total: u64,
    pub done: u64,
    pub kept: u64,
}
keys [done, kept, total]
}

impl Fns {
    pub fn compiled(self) -> u64 {
        self.done - self.kept.min(self.done)
    }
}

/// Why something was compiled again.
// clone: as `Record`; the UI keeps the causes it is told for the summary.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// Nothing was kept of it: no store entry, or no store.
    Cold,
    /// A file it is written in changed.
    Edited { file: PathBuf },
    /// Its body changed, and nothing it is seen by.
    Body,
    /// Its signature changed: what calls it is compiled again too.
    Signature,
    /// A function it inlines or calls by its code changed.
    Callee { name: String },
    /// Something it is compiled from changed, in a file other than its own: a type, a
    /// constant, a function it inlines.
    Uses,
    /// A dependency is of another version, features or flags.
    Dependency { name: String },
    /// What it is compiled with changed: the toolchain, a flag, a variable.
    Flags { what: String },
}

impl Cause {
    /// A few words for a summary line: "body edited", "call it".
    pub fn short(&self) -> String {
        // alloc: a string handed out; `Cause::shown` writes where it goes.
        self.shown().to_string()
    }

    /// [`Cause::short`], written straight into a formatter.
    pub fn shown(&self) -> impl fmt::Display + '_ {
        Short(self)
    }
}

struct Short<'a>(&'a Cause);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Cause::Cold => f.write_str("not kept"),
            Cause::Edited { file } => write!(f, "{} edited", file.display()),
            Cause::Body => f.write_str("body edited"),
            Cause::Signature => f.write_str("signature changed"),
            Cause::Callee { name } => write!(f, "calls {name}"),
            Cause::Uses => f.write_str("uses what changed"),
            Cause::Dependency { name } => write!(f, "{name} changed"),
            Cause::Flags { what } => write!(f, "{what} changed"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Note,
    Warning,
    Error,
}

crate::in_key_order! {
/// What a finished build did, for its last line.
// clone: as `Record`, whose `Event::Finished` holds it.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub took_ns: u64,
    pub crates: u64,
    pub fns: Fns,
    /// What was compiled again and why, the first few.
    pub rebuilt: Vec<(String, Cause)>,
    /// How many more there were than `rebuilt` lists.
    pub more: u64,
    pub error: Option<String>,
    /// What the store had, of a build that compiled.
    pub cache: Option<Cache>,
    /// The steps that took longest, longest first: why the build took as long as it did.
    pub slowest: Vec<Took>,
}
keys [fns, more, cache, error, crates, rebuilt, slowest, took_ns]
}

crate::in_key_order! {
/// The entries of compiled code a build asked the store for: found on this machine, fetched
/// from the remote store, or compiled because neither had them.
// clone: as `Record`; the UI keeps the last one for the summary.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Cache {
    pub found: u64,
    pub fetched: u64,
    pub compiled: u64,
    /// Why the store had none of the build, when it had none: no store, an empty one, or one
    /// of another blaze whose entries are not this one's.
    pub cold: Option<String>,
}
keys [cold, found, fetched, compiled]
}

crate::in_key_order! {
/// How long a step took, of what.
// clone: as `Summary`, which holds it.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Took {
    pub step: Step,
    pub of: Option<String>,
    pub took_ns: u64,
}
keys [of, step, took_ns]
}

/// What a client asks the daemon of builds.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum Subscribe {
    /// The records of the build under way in the workspace at `root`, or the last one, from
    /// its first, then each as it comes until it finishes.
    Build { root: PathBuf },
    /// Every build's, as they come, until the client hangs up.
    All,
}
