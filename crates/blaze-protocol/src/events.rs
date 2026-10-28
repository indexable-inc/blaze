//! What a build says as it goes, for whoever watches it: `blaze build` and `run` render it
//! live, `blaze status` and `watch` attach to a build another terminal started, `--events`
//! writes it for tools, and `blaze why` reads the last build's. The daemon keeps each
//! build's records in order; the same build gives the same records but for their times.
//!
//! Times are nanoseconds since the build began and counts are integers: no floats.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One thing a build said, `at_ns` after it began.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub at_ns: u64,
    pub event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A build began: the command that asked for it, in the workspace at `root`.
    Began { build: u64, command: String, root: PathBuf },
    /// A step of the build began, of what when it is of one crate or program.
    Started { step: Step, of: Option<String> },
    /// The step began last ended.
    Ended { step: Step, of: Option<String> },
    /// How far the step under way is: `done` of `total` of what it counts ([`Step::counts`]),
    /// `total` 0 while it is not known, and what of it has taken longest so far.
    Progress { step: Step, done: u64, total: u64, longest: Option<String> },
    /// How far the build of a crate is.
    Crate { name: String, state: CrateState, fns: Fns },
    /// Why a unit or a function was compiled again rather than taken as it was.
    Rebuilt { name: String, krate: String, cause: Cause },
    /// How many more functions were compiled again than the [`Event::Rebuilt`] records name.
    Unlisted { count: u64 },
    /// What the store had of the build's compile, and when it had little, why.
    Cache { cache: Cache },
    /// What blaze does not build, which fails the build (docs/architecture.md, "Never
    /// guess; refuse visibly").
    Refused { name: String, why: String },
    /// A line that is no step: a warning, a note.
    Said { level: Level, text: String },
    /// The build is done.
    Finished { ok: bool, summary: Summary },
    /// A node of the build's graph (docs/driver.md), said once, before it first moves: what it
    /// is and the nodes it waits for.
    Node { id: u32, kind: NodeKind, name: String, deps: Vec<u32> },
    /// A node of the graph is now in `state`.
    Moved { id: u32, state: NodeState },
    /// What held the build up: each node of it is the one the next waited for last, from the
    /// first to the one that finished last.
    Critical { path: Vec<u32> },
    /// How busy the process was since it said this last, while at a step: `busy` hundredths
    /// of a core, of the `of` cores it may run on. Said when it moved by a tenth of a core.
    Cores { busy: u32, of: u32 },
    /// The functions compiling for longer than a moment ([`SLOW_NS`]), longest first, each
    /// for `ns` by this record: said when which they are changes, and empty once none is.
    Compiling { functions: Vec<Slow> },
    /// The functions the build took [`SLOW_NS`] or more to compile, longest first, each in
    /// `ns`: said once, as it ends.
    Slowest { functions: Vec<Slow> },
    /// Functions put in object `object` of the process's builds, as they were put in it. An
    /// object is `whole`, all of the program, or a patch of the last whole one with what an
    /// edit compiled again. A function is in one record of an object. The daemon sends these
    /// to no command: a cold build's are some megabytes.
    Functions { object: u32, whole: bool, functions: Vec<Function> },
}

/// A function compiling longer than this is a slow one: [`Event::Compiling`] says it.
pub const SLOW_NS: u64 = 200_000_000;

/// A function slow to compile, of crate `krate`: `ns` is how long it took, or has so far.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Slow {
    pub krate: String,
    pub name: String,
    pub ns: u64,
}

/// A function or instance of the program a build writes: what the build page draws.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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

/// Where a node of the build's graph is. A node only moves down this list, from `Waiting`
/// to one of the last four.
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

/// The functions of a crate: how many the build has, how many are done, and of those how
/// many came from the store or were unchanged rather than compiled.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fns {
    pub total: u64,
    pub done: u64,
    pub kept: u64,
}

impl Fns {
    pub fn compiled(self) -> u64 {
        self.done - self.kept.min(self.done)
    }
}

/// Why something was compiled again.
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
        match self {
            Cause::Cold => "not kept".to_owned(),
            Cause::Edited { file } => format!("{} edited", file.display()),
            Cause::Body => "body edited".to_owned(),
            Cause::Signature => "signature changed".to_owned(),
            Cause::Callee { name } => format!("calls {name}"),
            Cause::Uses => "uses what changed".to_owned(),
            Cause::Dependency { name } => format!("{name} changed"),
            Cause::Flags { what } => format!("{what} changed"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Note,
    Warning,
    Error,
}

/// What a finished build did, for its last line.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
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

/// The entries of compiled code a build asked the store for: found on this machine, fetched
/// from the remote store, or compiled because neither had them.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Cache {
    pub found: u64,
    pub fetched: u64,
    pub compiled: u64,
    /// Why the store had none of the build, when it had none: no store, an empty one, or one
    /// of another blaze whose entries are not this one's.
    pub cold: Option<String>,
}

/// How long a step took, of what.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Took {
    pub step: Step,
    pub of: Option<String>,
    pub took_ns: u64,
}

/// What a client asks the daemon of builds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Subscribe {
    /// The records of the build under way in the workspace at `root`, or the last one, from
    /// its first, then each as it comes until it finishes.
    Build { root: PathBuf },
    /// Every build's, as they come, until the client hangs up.
    All,
}
