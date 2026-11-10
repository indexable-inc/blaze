//! The agent API (docs/v2/plan.md, docs/v2/plan.md section 1; #277; rule no-mcp): the daemon's
//! only interface.
//!
//! Every message is a [`Envelope`] of [`SCHEMA`], encoded in RFC 8949's core deterministic
//! CBOR ([`blaze_protocol::cbor`]) and framed as [`blaze_protocol::write_frame`] frames it, over
//! a Unix socket or the body of an HTTP request. A reader of another schema refuses the
//! message by name. One connection carries many requests; each [`Response`] names its
//! request's id, and a request may have many (events, then its end).
//!
//! The shapes are for LLMs: whole answers, no paging cursors to keep, every error naming what
//! and where.
//!
//! Every struct and struct variant here declares its fields in the order of their keys'
//! encoding (shorter names first, then bytewise), the order the CBOR has them in, so each is
//! written as it comes with nothing to sort. The bytes are the same in any order; JSON shows
//! fields in this one.
//!
//! - [`frames`]: frames in and out of a byte stream, and the schema check.
//! - [`borrowed`]: a response read where it lies in the frame ([`ResponseRef`]), for the CLI.
//! - [`http`]: HTTP/1.1, sans-IO, for the API's HTTP side and the webhook.
//! - [`server`]: the daemon's listener, as a state machine over [`blaze_core`]'s sockets: what
//!   [`Arrived`] and where each [`Reply`] goes.
//! - [`session`]: what each request does, over the VFS, the database and the [`build::Builder`].
//! - [`client`]: the CLI's and CI's side, blocking.
//! - [`json`]: the rendering for people.
//! - [`activation`]: the socket a service manager passes the daemon, the one unsafe module.
//! - [`units`]: the launchd agent and systemd units that socket-activate it.

#![deny(unsafe_code)]
#![feature(allocator_api)]

pub mod activation;
pub mod borrowed;
pub mod build;
pub mod client;
pub mod frames;
pub mod http;
pub mod intent;
pub mod json;
pub mod server;
pub mod session;
pub mod units;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use blaze_protocol::events::NodeKind;
pub use blaze_report::{Blocks, Fact, Fix, Report, Severity, Span as ReportSpan, Text};
pub use blaze_vfs::{Mode, Op};
pub use borrowed::{
    BasisRef, BuildScriptRanRef, CachedRef, DiagnosticRef, FactReadRef, LiveFrameRef, ResponseRef,
    SpanRef, TestResultRef, UnitOfRef, UnitStartedRef, UnitTargetRef,
};
pub use intent::{Edit, Edited, EditedItem, How, Target};

use blaze_core::Conn;
use blaze_ids::{ContentId, Fingerprint};
use blaze_query::{QueryKey, Why};
use blaze_vfs::{Change, View};
use serde::{Deserialize, Serialize};

/// The version of every body's schema. A change to any type here is a new number. 11: every
/// `Vec<u8>` (a run's output, a test's, cargo's files) is one byte string, not an array. 12:
/// [`Request::Memory`] and [`Response::Memory`]. 13: [`Op`]'s bytes are one byte string too, and so is each [`ContentId`] and
/// [`Fingerprint`]. 14: [`Cargo::width`], [`Cargo::verbose`], [`Diagnostic::explanation`] and
/// [`TestResult::outside`]; [`Worked::served`]; dense ids
/// ([`UnitOf::id`], [`UnitStarted::package`], [`TestResult::executable`]); the fixed name sets
/// as enums ([`TargetKind`], [`CrateType`], [`ArtifactKind`], [`Applicability`]), written by
/// name as before. [`ResponseRef`] reads the same bytes. 15: `Request::Pack` and
/// `Response::Packed`. 16: [`Worked::links`]. 17: [`Worked::steps_us`]. 18: [`Request::Doc`],
/// [`Response::DocPage`] and [`Response::Documented`]. 19: [`Cargo::refused`]. 20:
/// [`Request::Explore`], [`Request::Profile`], [`Request::Live`], `Request::Sql`, their
/// answers, and [`Status::history`]. 21: [`Edit::Replace`]. 22: [`Status::build`]. 23:
/// [`Request::Hot`] and [`Response::Hot`]. 24: [`Request::Gc`] and [`Response::Collected`]. 25:
/// [`Response::UnitBegan`] and [`Response::TestReady`], which let `blaze test` run a test
/// executable while the rest of the build goes on. 26: [`Request::Rebuilt`] and
/// [`Response::Rebuilt`] (`blaze why`), grouped on the daemon. 27: [`Worked::placed`]. 28:
/// [`Request::Launch`] and [`Response::Launched`]. 29: [`Response::Stopping`]. 30:
/// [`UnitOf::doctest`]. 31: [`Response::Unchecked`]. 32: [`LinkHow::Unchanged`]. 33:
/// [`Status::bugs`]. 34: [`Request::FetchUrl`] and [`Response::Stored`]. 35:
/// [`Status::settings`]. 36: an [`ApiError`] is its [`Report`] (docs/v2/plan/design/structured-output.md),
/// [`Response::Report`], and [`Behind::id`]. 37: [`RebuildCause::Unfinished`]. 38:
/// [`Request::Flush`] and [`Response::Flushed`] in place of `Request::Sql` (`blaze debug sql
/// --live`) and `Response::Queried`: a reader of the profile history asks the daemon to flush,
/// then reads the history itself; and [`Request::ClientRows`] and [`Worked::build_id`]: a client
/// sends its own recorder's rows of a build to the daemon, which writes them to the history. 39:
/// [`Request::Lock`]. 40: [`Status::uptime_ms`], for `blaze status --json`. 41:
/// [`Response::Doing`] (what a build waits on and what each unit under way does, about once a
/// second), [`Status::builds`] and [`Status::downloads`]. 42: [`Doing::now`] and
/// [`Building::now`], the build's live model (`blaze_protocol::now::Now`). 43: `Now` gains
/// its running tasks, longest first (`running`), the tasks settled in the last 30 s
/// (`recent`), the failed tasks (`failures`) and the proc-macro expansion lane (`expanding`).
/// 44: `Now::stall`, the daemon's top stall in its wait-for graph (a deadlock or a convoy of
/// named locks), in one line, which `blaze status` and the build view say. 45:
/// [`Rebuild::ends`], the terminal reasons ([`Reason`]) a recomputed step's chain ends in,
/// [`RebuildCause::Dropped`], and [`PartChanged::kind`] and [`PartChanged::trivia`]. 46:
/// [`Rebuilt::summary`] (what a build says of itself once it ends), and `build` and
/// `unexplained` of [`Request::Rebuilt`]. 47: [`Worked::misses`], by which a build that
/// recomputed nothing asks for no summary. 48: `Request::Pack` and `Response::Packed` are gone
/// with the in-browser compiler they fed.
pub const SCHEMA: u32 = 48;

/// The most bytes a request the daemon reads may be: CBOR from a peer is decoded only up to
/// this.
pub const LONGEST_REQUEST: u32 = blaze_protocol::LONGEST_FRAME;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Envelope<T> {
    pub schema: u32,
    /// Chosen by the client; a response carries the id of its request.
    pub id: u64,
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(id: u64, body: T) -> Envelope<T> {
        Envelope { schema: SCHEMA, id, body }
    }
}

/// A view as a request names it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum ViewRef {
    /// A head (a workspace, a branch).
    Head(String),
    /// A root, by content ID.
    Root(View),
    /// A head or root with ops applied, moving nothing: a what-if.
    WhatIf { ops: Vec<Op>, base: Box<ViewRef> },
}

/// What to build or run.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Targets {
    /// Everything a cargo command line says beyond the fields below, for `blaze build`,
    /// `run`, `test`, `check` and `cargo blaze`; `None` for an agent's request, built as cargo
    /// builds the fields below with its defaults. Where both say something, this wins. A
    /// check with cargo's `--keep-going` checks every member past one that fails.
    pub cargo: Option<Cargo>,
    /// A target triple; `None` for the machine's.
    pub target: Option<String>,
    pub profile: String,
    pub features: Vec<String>,
    /// Packages by name; empty for the workspace's default members.
    pub packages: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

/// A cargo command line, as the CLI was run.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Cargo {
    /// The directory the command ran in, absolute.
    pub cwd: String,
    /// The command's environment, sorted by name. Held in memory for the request only: no
    /// query value, log or store entry holds a variable's value unless cargo reads it by name.
    pub env: Vec<(String, String)>,
    /// The arguments after the subcommand and before `--`, as typed.
    pub args: Vec<String>,
    /// Where the view's root is on the client's machine, absolute: what cargo's paths
    /// (`CARGO_MANIFEST_DIR`, `file!()`, the target directory) are under.
    pub root: String,
    /// Whether diagnostics are rendered with colors (cargo's `--color`, resolved by the
    /// client against its terminal).
    pub color: bool,
    /// The files outside the view cargo reads, by absolute path: `$CARGO_HOME/config.toml`,
    /// the `.cargo/config.toml` of each directory above the root, a `--config` file outside
    /// it. Never credentials.
    #[serde(with = "files")]
    pub files: Vec<(String, Vec<u8>)>,
    /// The columns diagnostics are rendered in: what cargo passes rustc as
    /// `--diagnostic-width` (`Shell::err_width().diagnostic_terminal_width()`, the terminal's
    /// when stderr is one); `None` for rustc's own default.
    pub width: Option<u32>,
    /// `build`, `check`, `test`, `bench` or `run`.
    pub command: String,
    /// How many of the functions blaze could not compile a build's error names: the first
    /// `n`, or with `None` every one (blaze's own `--refused N|all`; 16 without it).
    pub refused: Option<u32>,
    /// How many `-v` were given (cargo's `Verbosity`: 1 is `Verbose`, 2 or more `-vv`); 0 with
    /// none or with `-q`, which `args` says.
    pub verbose: u8,
}

/// [`Cargo::files`], each file's bytes one byte string.
mod files {
    use blaze_protocol::bytes::{Buf, Bytes};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(files: &[(String, Vec<u8>)], to: S) -> Result<S::Ok, S::Error> {
        to.collect_seq(files.iter().map(|(path, bytes)| (path, Bytes(bytes))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        from: D,
    ) -> Result<Vec<(String, Vec<u8>)>, D::Error> {
        let files = Vec::<(String, Buf)>::deserialize(from)?;
        // alloc: none past the list's own; the same layout is collected in place.
        Ok(files.into_iter().map(|(path, Buf(bytes))| (path, bytes)).collect())
    }
}

/// The environment's values may be secrets: Debug names the variables only.
impl std::fmt::Debug for Cargo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cargo")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("root", &self.root)
            .field("cwd", &self.cwd)
            .field("env", &Names(&self.env))
            .field("files", &Names(&self.files))
            .field("color", &self.color)
            .field("width", &self.width)
            .field("verbose", &self.verbose)
            .field("refused", &self.refused)
            .finish()
    }
}

/// The first of each pair, as a Debug list: names, never values.
struct Names<'a, T>(&'a [(String, T)]);

impl<T> std::fmt::Debug for Names<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.iter().map(|(name, _)| name)).finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Request {
    /// Applies ops to a head, rebased if it moved; answered with [`Response::Committed`].
    Commit {
        ops: Vec<Op>,
        base: ContentId,
        head: String,
    },
    /// Checks the view; answered with diagnostics.
    Check {
        view: ViewRef,
        targets: Targets,
    },
    Build {
        view: ViewRef,
        targets: Targets,
    },
    /// Runs the tests the view's change reached (all, with `all`), filtered by name, with
    /// libtest's selection `args`; in blaze's MIR evaluator, checked for undefined behavior,
    /// with `ub`.
    Test {
        ub: Option<Ub>,
        all: bool,
        args: Vec<String>,
        view: ViewRef,
        filter: Vec<String>,
        targets: Targets,
    },
    Run {
        bin: Option<String>,
        args: Vec<String>,
        view: ViewRef,
        targets: Targets,
    },
    /// The files that differ between two views.
    Diff {
        to: ViewRef,
        from: ViewRef,
    },
    /// Why a query reran in the last build of a view.
    Why {
        view: ViewRef,
        query: QueryKey,
    },
    /// What the daemon is doing: its requests, memory, views.
    Status,
    /// Cancels the request `id` of this connection.
    Cancel {
        id: u64,
    },
    /// Resolves a view to its root and keeps it open until [`Request::Close`] or the
    /// connection ends; answered with [`Response::Opened`].
    Open {
        view: ViewRef,
    },
    Close {
        root: ContentId,
    },
    /// What the change from `from` to `to` reaches (items, facts, functions compiled again,
    /// tests), building only each view's proc macros and build scripts' runs, which its front
    /// end applies before it reads as rustc compiles; answered with [`Response::Reached`].
    Reach {
        to: ViewRef,
        from: ViewRef,
        targets: Targets,
    },
    /// Each candidate's ops applied to `base` as a what-if, checked (and tested, with `test`),
    /// all at once: each candidate's responses as [`Response::Of`], then [`Response::Ranked`].
    BestOf {
        base: ViewRef,
        test: bool,
        filter: Vec<String>,
        targets: Targets,
        candidates: Vec<Vec<Op>>,
    },
    /// The production oracle: the view built again from nothing and compared; answered with
    /// [`Response::Verified`].
    Verify {
        view: ViewRef,
        targets: Targets,
    },
    /// The daemon's state made durable, before the VM snapshots itself; answered with
    /// [`Response::Snapshotted`] once it is.
    Snapshot,
    /// Every instance the view's last build compiled or found, with the hash its code was
    /// made from; answered with [`Response::CodegenInputs`].
    CodegenInputs {
        view: ViewRef,
        targets: Targets,
    },
    /// Every result the view's last build ran or kept, with its edges; answered with
    /// [`Response::Trace`].
    Trace {
        view: ViewRef,
        targets: Targets,
    },
    /// The size of each of the daemon's long-lived tables; answered with [`Response::Tables`].
    Tables {
        view: ViewRef,
    },
    /// `blaze stop`: the daemon's state made durable, then it exits; answered with
    /// `Done { ok: true }` before it does.
    Stop,
    /// Edits by meaning, applied in the front end to `base` and committed to `head` as
    /// [`Request::Commit`] commits: answered with [`Response::Edited`], then
    /// [`Response::Committed`] once durable. The view's proc macros and build scripts' runs,
    /// which its front end applies before it resolves a name as rustc would, are built first.
    Edit {
        base: ContentId,
        head: String,
        edits: Vec<Edit>,
    },
    /// Declared intent: the tests `filter` selects in the view will be run soon. What they
    /// reach is built ahead at a priority below every request's, answered at once with
    /// [`Response::Announced`]. A request on the same root takes the work over; a newer intent
    /// of this connection, the connection's end, or `wait_ms` with no such request cancels it.
    /// The view's proc macros and build scripts' runs, which its front end applies before it
    /// lists a unit's tests, are built before the tests are chosen.
    WillTest {
        view: ViewRef,
        filter: Vec<String>,
        targets: Targets,
        wait_ms: u64,
    },
    /// Declared intent: `ops` will be committed to `head`. The what-if is checked (tested,
    /// with `test`) ahead as [`Request::WillTest`] builds ahead; a commit to `head` that makes
    /// the same root takes the work over, and one that makes another root cancels it.
    WillSave {
        ops: Vec<Op>,
        head: String,
        test: bool,
        targets: Targets,
        wait_ms: u64,
    },
    /// What each subsystem's memory is doing, from blaze-mem's counters; answered with
    /// [`Response::Memory`].
    Memory,
    /// `cargo doc`: the targets it documents checked as `targets` asks (`Targets::cargo`'s
    /// command `doc`), then the HTML documentation of those crates and, unless `no_deps`, of
    /// every library they depend on, written by blaze from the front end (`blaze-doc`), no
    /// rustdoc run. `private` is `--document-private-items`. Answered with
    /// [`Response::Documented`], then each file as a [`Response::DocPage`], which the client
    /// writes, then rustdoc's warnings as [`Response::Diagnostic`]s.
    Doc {
        view: ViewRef,
        no_deps: bool,
        private: bool,
        targets: Targets,
    },
    /// `blaze explore`: the explorer's server started in the daemon if it is not yet (with
    /// `dev`, also accepting the page's development server), answered with
    /// [`Response::Explored`]. Answered by the daemon's loop, which holds the server.
    Explore {
        dev: bool,
    },
    /// `blaze debug profile`: a Chrome trace of the last `seconds` of every thread's flight
    /// recorder and the CPU samples; answered with [`Response::Profiled`].
    Profile {
        seconds: u32,
    },
    /// `blaze debug live`: a [`Response::Live`] every 250 ms until the connection ends. A
    /// subscriber whose connection has a frame not yet sent is skipped that tick; the next
    /// frame has every row since its last. Answered by the daemon's loop, which holds the feeds.
    Live,
    /// A reader of the profile history (`blaze debug sql`, `blaze slow`, `blaze debug time`,
    /// `blaze explain`) asks the daemon that writes it to flush what it has recorded, so the
    /// history's manifest lists every row recorded before the ask; answered with
    /// [`Response::Flushed`] once that flush has committed. Answered by the daemon's loop,
    /// which hands it to the history's keeper.
    Flush,
    /// `blaze debug hot`: the `top` keys (a span and what it is of) that took most time and
    /// each kind of wait's quantiles, from the daemon's live sketches (blaze-profile's `hot`),
    /// no history read; answered with [`Response::Hot`].
    Hot {
        top: u32,
        since: HotSince,
    },
    /// `blaze cache gc` while the daemon holds the collector's areas: one run of the daemon's
    /// collector to its end (`dry_run` plans and removes nothing), answered with
    /// [`Response::Collected`] as it records. Answered by the daemon's loop, which holds the
    /// collector.
    Gc {
        dry_run: bool,
    },
    /// `blaze why [ITEM]`: what the last build that ended in this daemon's life computed again
    /// instead of reusing, grouped by step and reason, each group with its first items (every
    /// one with `all`); with `item`, that item's reasons and, after them, those of each item a
    /// reason names, in order (its chain). Of the last build of the workspace holding `cwd`
    /// (an absolute path), or with none the newest build's. Answered with
    /// [`Response::Rebuilt`] by the daemon's loop, which keeps each workspace's last reasons.
    /// With `build` (a build's own client, once it has its `Done`), of that build alone,
    /// whatever `cwd`: answered with no build when the reasons kept are another's. With
    /// `unexplained`, only the steps whose chain ends in [`Reason::Unexplained`], each one
    /// (`blaze why --unexplained`).
    Rebuilt {
        all: bool,
        cwd: Option<String>,
        item: Option<String>,
        build: Option<u64>,
        unexplained: bool,
    },
    /// `blaze run` of a built program, before it execs it as process `pid`: what that process
    /// needs to run on the daemon's code memory when `program` (the path the build linked it
    /// at) is a dev executable the daemon holds (docs/v2/plan/design/compile-time.md).
    /// Answered with [`Response::Launched`] by the daemon's loop, which holds the programs.
    Launch {
        pid: u32,
        program: String,
    },
    /// `blaze fetch-url '<json>'`: the bytes of `url`, or the tree of the git repository `repo`
    /// at the full commit `rev`, whose digest is `digest` (`sha256:<hex>`, `sha512:<hex>` or
    /// `blake3:<hex>`; a git tree's over its canonical listing): from the daemon's store when it
    /// has them, else fetched, checked and kept there (blaze-fetch). Exactly one of `url` and
    /// `repo` (with `rev`). `most_bytes` bounds the download (blaze-fetch's
    /// `DEFAULT_MOST_BYTES` when `None`); `out`, an absolute directory, is where to write the
    /// bytes out, named by their BLAKE3 (with none, they are only kept in the store). Answered
    /// with [`Response::Stored`] by the daemon's loop, which holds the store.
    FetchUrl {
        out: Option<String>,
        rev: Option<String>,
        url: Option<String>,
        repo: Option<String>,
        digest: String,
        most_bytes: Option<u64>,
    },
    /// `blaze lock`: the workspace resolved as `targets` asks and its lock files (`Cargo.lock`,
    /// `blaze.lock`) written when they change, whatever `CI` or `lock = "required"` says
    /// (`Targets::cargo` carries `--no-locked`); nothing checked or built, no build script run.
    /// Answered with what it wrote as note [`Response::Diagnostic`]s, or the load's error.
    Lock { view: ViewRef, targets: Targets },
    /// What a client recorded of build `build` (its [`Worked::build_id`]), sent as the request
    /// that asked for it ends: its own `spans`, `events` and `folded` rows, each an Arrow IPC
    /// stream of the profile schema's table (empty for none), every row's `build_id` the
    /// build's, and when it started and had its answer, on the profile's clock
    /// (`blaze_profile::clock::mono_ns`). The daemon's loop hands them to its profile history,
    /// which writes them as the client's process and gives the build's row the two times
    /// (`builds.client_started_ns`, `client_ended_ns`); answered with `Done { ok: true }` once
    /// written. A daemon that keeps no history answers at once and keeps nothing.
    ClientRows {
        build: u64,
        #[serde(with = "blaze_protocol::bytes")]
        spans: Vec<u8>,
        #[serde(with = "blaze_protocol::bytes")]
        events: Vec<u8>,
        #[serde(with = "blaze_protocol::bytes")]
        folded: Vec<u8>,
        ended_ns: i64,
        started_ns: i64,
    },
}

/// [`Request::Rebuilt`]'s answer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Rebuilt {
    /// Why what the build is compared against is short, when it is: blaze why's ledger did
    /// not restore whole at the daemon's start, so an item it lost reads as computed first.
    pub torn: Option<String>,
    /// The build they are of: the workspace's last (or the newest) that ended since the daemon
    /// started; none when none has (an earlier daemon's builds are in the history's `misses`
    /// table).
    pub build: Option<u64>,
    /// Whether an item was asked and found, so that `rebuilt` is its chain: its own reasons
    /// first, then each item one of them names, in the order they are reached.
    pub chain: bool,
    /// How many things the build computed again, of those asked of (every one, or those whose
    /// item starts with the one asked).
    pub count: u32,
    /// Without a chain: what was computed again by step and reason, the most items first.
    pub groups: Vec<RebuildGroup>,
    /// The build's command line, when there is a build.
    pub command: Option<String>,
    /// Subjects last computed in this build or before were dropped from blaze why's ledger to
    /// keep it under its cap: an item read as computed first may be one of them.
    pub dropped: Option<u64>,
    /// With a chain: its reasons in order. Asked for the unexplained: each such step.
    pub rebuilt: Vec<Rebuild>,
    /// Without an item: what the build says of itself once it ends
    /// (docs/v2/plan/design/why.md, "What is printed"): a report a root cause, the most
    /// steps first, at most four and then one counting the rest; empty for a build that
    /// recomputed nothing.
    pub summary: Vec<Report>,
    /// Whether the daemon is still restoring blaze why's ledger (off its start path), so what
    /// `torn` says is not known yet.
    pub restoring: bool,
    /// The build's workspace, when there is a build.
    pub workspace: Option<String>,
}

/// Things one build computed again for one reason.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RebuildGroup {
    /// What was computed again: `codegen dev template`, `unit`, `build script run`.
    pub step: String,
    /// The reason, as its first item has it: what each item of the group shares is its kind
    /// and what it names (the parts, the read), not the build it is compared with.
    pub cause: RebuildCause,
    /// How many items the group has.
    pub count: u32,
    /// Its items by path, sorted: the first 8, or every one when asked for all.
    pub items: Vec<String>,
}

/// One thing a build computed again, and why.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Rebuild {
    /// What its chain of reasons ends in (docs/v2/plan/design/why.md): its own `cause`
    /// followed through each part that is another step's result, down to inputs that changed
    /// or another terminal reason. Sorted, each once, the first 8.
    pub ends: Vec<Reason>,
    /// What it is of, by path: a function, a unit (`app v0.1.0 (lib)`), a package's build
    /// script's run. Only for a person: two of one name (one function built for a test and
    /// for a program) are told apart by blaze, not by it.
    pub item: String,
    /// What was computed again: `codegen dev template`, `unit`, `build script run`.
    pub step: String,
    pub cause: RebuildCause,
}

/// Why something was computed again rather than reused.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum RebuildCause {
    /// Nothing was kept under its key, and blaze has no record of computing it before.
    First,
    /// Parts of its key changed since it was last computed, in build `since` (the last build
    /// that computed it and ended well; builds between may have reused it under another key):
    /// each one that changed, of the key's `parts`.
    Parts { parts: u32, since: u64, changed: Vec<PartChanged> },
    /// Its key is the one it had when last computed, in build `since`, yet nothing is kept
    /// under it: that compute did not keep it.
    Lost { since: u64 },
    /// Its key is the one it had when last computed, in build `since`, which did not end well
    /// (it failed or was stopped), and nothing is kept under it: that compute likely failed or
    /// was stopped before it kept anything.
    Unfinished { since: u64 },
    /// It was last computed in build `since`, and that is all blaze why's ledger kept of it:
    /// its record's parts were dropped to keep the ledger under its cap, so what changed
    /// since is not known.
    Dropped { since: u64 },
    /// Its key is the one it had when last computed, in build `since`, so what its value is
    /// filed under past the key differs, or that compute did not keep it: `what` is filed
    /// beyond the key, which blaze does not explain yet.
    Beyond { what: String, since: u64 },
    /// Its key differs from build `since`'s, yet every part it names is the same: the key has
    /// a part it does not name.
    Unnamed { since: u64 },
    /// A read the entry kept last recorded hashes differently now: the first such, named by
    /// path, the hashes it saw then and sees now (none: it names nothing), and `changed` of
    /// the entry's `reads` that changed; `since` is the build that kept that entry.
    Read {
        now: Option<ContentId>,
        was: Option<ContentId>,
        read: String,
        names: Option<String>,
        reads: u32,
        since: Option<u64>,
        changed: u32,
    },
    /// Shapes of its reads are kept, but blaze has no record of the entry it kept last: kept
    /// by another daemon, or before its ledger began.
    Unrecorded,
    /// The entry blaze recorded as kept last is not in the store: that compute did not keep
    /// it.
    Collected,
    /// Every read of the entry kept last hashes as it did, yet no shape of reads names it: a
    /// keep that did not finish.
    Unshaped,
    /// The build computed it `count` times under different keys, so none is compared with its
    /// last compute: blaze told the instances apart by less than their keys (a bug in blaze).
    Twice { count: u32 },
    /// It is computed every time: why.
    Always { why: String },
    /// Its key does not expose its parts yet: the kind of key.
    NotExplained { what: String },
    /// The reason could not be worked out: what failed.
    Failed { why: String },
}

/// Where a recomputed step's chain of reasons ends: a closed set. Anything blaze cannot put
/// in one of the others is [`Reason::Unexplained`], which is a bug in blaze and said as one.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Reason {
    /// An input of the build changed: `input` names it (a file by its absolute path, when
    /// `file`; else a variable, a flag, a setting, the toolchain). `trivia`: a file whose
    /// bytes changed and whose code did not (comments only), so no item changed and the
    /// recompute was wasted work.
    InputChanged { file: bool, input: String, trivia: bool },
    /// blaze has no record of ever computing it.
    FirstTime,
    /// It was computed before, last in build `when`, and its result or its record was dropped
    /// since, by what `by` says.
    Evicted { by: EvictedBy, when: u64 },
    /// Its key is the one of its last compute, in build `since`, which failed or was stopped
    /// and kept nothing.
    PreviousFailed { since: u64 },
    /// It was computed because that was asked for: how.
    Forced { how: String },
    /// `part`, another step's result, differs though that step has not been computed since:
    /// the same inputs gave another value (a determinism bug in blaze).
    KeyUnstable { part: String },
    /// No recorded reason: what is missing. A bug in blaze.
    Unexplained { what: String },
}

/// What dropped a result or its record ([`Reason::Evicted`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum EvictedBy {
    /// The store, to stay under its memory.
    Memory,
    /// The store, as unused for too long.
    Age,
    /// `blaze why`'s ledger, to stay under its cap: the record's parts, not the result.
    Cap,
}

/// A part of a key that changed: its hash then and now (none: absent), its name, and the item
/// it is of, when another's (what a chain follows).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PartChanged {
    pub now: Option<ContentId>,
    pub was: Option<ContentId>,
    /// What the part is: `input`, `file` (then `part` is its absolute path) or `result`
    /// (another step's value, which a chain follows).
    pub kind: PartIs,
    pub part: String,
    pub names: Option<String>,
    /// For a file whose bytes changed: whether what code can observe of it did not (only
    /// comments changed, so no item did).
    pub trivia: bool,
}

/// What a part of a key is ([`PartChanged::kind`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum PartIs {
    Input,
    File,
    Result,
}

/// Which stretch of the daemon's life a [`Request::Hot`] sums.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotSince {
    /// Since the daemon began.
    Start,
    /// From the end of the build before the last to the end of the last.
    LastBuild,
}

/// [`Request::Hot`]'s answer. Times are of spans nested in others too, so they add up to more
/// than the wall time.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Hot {
    /// Most time first.
    pub keys: Vec<HotKey>,
    /// List entries read while a thread replaced them: their exact time is left out of
    /// [`HotKey::lower_ns`].
    pub torn: u64,
    pub since: HotSince,
    /// Spans counted.
    pub spans: u64,
    /// Each kind of wait that had any.
    pub waits: Vec<HotWait>,
    /// How far over the truth a [`HotKey::upper_ns`] may be, but for a chance of 1.8%.
    pub error_ns: u64,
    pub total_ns: u64,
}

/// A key and its time: never more than `upper_ns` (a count-min estimate) and never less than
/// `lower_ns`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct HotKey {
    /// The span's name: a query kind, or a step of the build.
    pub span: String,
    pub function: HotFunction,
    pub lower_ns: u64,
    pub upper_ns: u64,
}

/// What a [`HotKey`]'s span is of.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum HotFunction {
    /// The span names nothing it is of.
    None,
    Named(String),
    /// The daemon let the name go (its flight recorder keeps two generations of names): the
    /// number it had.
    Forgotten(u32),
}

/// One kind of wait: how many there were and their quantiles, each within 1% of the exact.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct HotWait {
    pub kind: HotWaitKind,
    pub count: u64,
    pub max_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
}

/// What a wait waits on: blaze-graph's `WaitOn` variants.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotWaitKind {
    Key,
    Lock,
    Channel,
    Io,
    Child,
    Scope,
}

/// One frame of [`Request::Live`]: blaze-profile's `live::Frame`, each table the next bytes of
/// the subscription's one long-lived Arrow IPC stream of the schema's table of that name
/// (`live::Encoder`; empty when it has no rows), which a client reads with one `live::Decoder`
/// a table across the frames, in order.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct LiveFrame {
    /// Spans not recorded since the daemon began.
    pub lost: u64,
    #[serde(with = "blaze_protocol::bytes")]
    pub open: Vec<u8>,
    /// When it was read, on the schema's clock.
    pub at_ns: i64,
    #[serde(with = "blaze_protocol::bytes")]
    pub spans: Vec<u8>,
    /// Rows written over before this subscriber read them, since its last frame.
    pub skipped: u64,
    #[serde(with = "blaze_protocol::bytes")]
    pub counters: Vec<u8>,
}

/// `blaze test --ub`: each selected test run under `hir_ty_ub::run_test`, nothing compiled.
/// `None` is `hir_ty_ub::Config::default()`'s.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ub {
    pub seed: Option<u64>,
    pub schedules: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Response {
    Committed {
        root: ContentId,
        rebased: bool,
    },
    /// A diagnostic, as rustc would give it, with its code.
    Diagnostic(Diagnostic),
    /// A build's output, placed. `profile` as cargo's `Finished` line names it:
    /// "`dev` profile [unoptimized + debuginfo]". `members` is the workspace's member count:
    /// cargo's `needs_spec_flag` puts `-p foo` in "to rerun pass" when there is more than one.
    Built {
        members: u32,
        profile: String,
        artifacts: Vec<Artifact>,
    },
    /// A unit began: cargo's `Compiling foo v0.1.0 (/path)` line.
    Unit(UnitStarted),
    /// A unit's compile ended, after every diagnostic of it: where cargo prints its
    /// "generated N warnings" or "could not compile".
    UnitEnded(UnitOf),
    /// cargo's `Updating crates.io index` / `Downloaded serde v1.0.219` as the daemon fetches
    /// what the load asked for: once for each source a job fetches, once for each index or
    /// git repository it updates.
    Fetched {
        verb: FetchVerb,
        what: String,
    },
    /// A build script ran: cargo's `build-script-executed` message.
    BuildScript(BuildScriptRan),
    /// What a build, test or run did, before its [`Response::Done`].
    Work(Worked),
    /// What a build that failed left unchecked behind its failures, and what it checked
    /// against a unit that failed only in its bodies or lints, before its [`Response::Built`].
    Unchecked(Unchecked),
    TestResult(TestResult),
    Ran {
        code: Option<i32>,
        #[serde(with = "blaze_protocol::bytes")]
        stderr: Vec<u8>,
        #[serde(with = "blaze_protocol::bytes")]
        stdout: Vec<u8>,
    },
    Diff {
        changes: Vec<Change>,
    },
    Why(Why),
    Status(Status),
    /// The request is over; `ok` false after an error or a failed build.
    Done {
        ok: bool,
    },
    Error(ApiError),
    Opened {
        root: ContentId,
    },
    Reached(Reach),
    /// A response of candidate `candidate` of a best-of-N.
    Of {
        response: Box<Response>,
        candidate: u32,
    },
    /// Best first.
    Ranked(Vec<Ranked>),
    Verified {
        mismatches: Vec<Mismatch>,
    },
    /// Every head's root, as made durable.
    Snapshotted {
        heads: Vec<(String, ContentId)>,
    },
    /// Each instance's key (`blaze_codegen::InstanceKey`) with the code hash
    /// (`blaze_items::CodeHash`) it was made from, sorted by key.
    CodegenInputs(Vec<(ContentId, ContentId)>),
    Trace(Vec<Traced>),
    Tables(Vec<Table>),
    /// What a [`Request::Edit`] changed, before its commit's [`Response::Committed`].
    Edited(Edited),
    /// The root a [`Request::WillTest`] or [`Request::WillSave`] builds ahead.
    Announced {
        root: ContentId,
    },
    Memory(Memory),
    /// A file of a [`Request::Doc`]'s documentation: its path under [`Documented::dir`],
    /// `/`-separated, and its bytes.
    DocPage {
        #[serde(with = "blaze_protocol::bytes")]
        page: Vec<u8>,
        path: String,
    },
    /// What a [`Request::Doc`] documents and where, before its first [`Response::DocPage`].
    Documented(Documented),
    /// The link of [`Request::Explore`]: `https://ix.dev/explore#127.0.0.1:<port>/<token>/<hash>`.
    /// Its fragment holds the session's token: printed to the terminal only, never logged.
    Explored {
        link: String,
    },
    /// [`Request::Profile`]'s Chrome trace (ui.perfetto.dev opens it).
    Profiled {
        #[serde(with = "blaze_protocol::bytes")]
        trace: Vec<u8>,
    },
    Live(LiveFrame),
    /// [`Request::Flush`]'s answer: the number of the flush that committed, which the
    /// history's manifest says from then on (`Manifest::flushed`).
    Flushed {
        seq: u64,
    },
    /// [`Request::Hot`]'s answer.
    Hot(Hot),
    /// [`Request::Gc`]'s answer: the run's report as `blaze cache gc` prints it (its line, each
    /// volume, each object's fate).
    Collected {
        report: String,
    },
    /// A unit's compile began: one for every unit, where [`Response::Unit`] is one for its
    /// package. Sent only for a cargo `test` or `bench` build, with a
    /// [`Response::UnitEnded`] for every unit too (not only those with diagnostics), so the
    /// command's live view shows each unit under way and done.
    UnitBegan(UnitOf),
    /// A test executable (or a doc test's) is linked and placed, ready to run before the build
    /// ends: sent only for a cargo `test` or `bench` build, and before its
    /// [`Response::Built`], which still lists it.
    TestReady(Artifact),
    /// [`Request::Rebuilt`]'s answer.
    Rebuilt(Rebuilt),
    /// [`Request::Launch`]'s answer: `live` when the program runs on the daemon's code memory
    /// and takes the build's edits while it runs, with the variables `env` its process is
    /// started with; otherwise it runs its file as linked, and `env` is empty.
    Launched {
        env: Vec<(String, String)>,
        live: bool,
    },
    /// After a [`Request::Stop`]'s [`Response::Done`], each step the daemon takes before it
    /// hangs up (making, writing and mapping its image), said as it begins: `blaze stop` waits
    /// for each step in turn and, when one outlasts its patience, names it.
    Stopping {
        doing: String,
    },
    /// [`Request::FetchUrl`]'s answer: the bytes' BLAKE3 (`blake3`, in hex), as the store
    /// keeps them, and, when the request named `out`, where they are written out, read-only
    /// and named by it: a file for a URL, a directory for a git tree.
    Stored {
        path: Option<String>,
        blake3: String,
    },
    /// Something blaze itself says, of a unit (`unit`) or of the request: a build script that
    /// failed, a directive the sandbox refused. A diagnostic of code is a
    /// [`Response::Diagnostic`]; an error that ends the request is a [`Response::Error`].
    Report {
        unit: Option<UnitOf>,
        report: Report,
    },
    /// What a build is doing, said about once a second while it runs: what it waits on while
    /// none of its units compiles (a download and how far it has come, the longest open span of
    /// the build, or the stage it is in), and what each unit under way is inside now. The
    /// command's live view shows it in place of a bare "compiling"; off a terminal its
    /// [`Doing::on`] is a line every few seconds.
    Doing(Doing),
}

/// [`Response::Doing`]. Its fields are declared in the order CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Doing {
    /// What the build waits on while none of its units compiles: `downloading
    /// llvm-23.1.4-aarch64-apple-darwin.tar.xz: 34.0 of 410.0 MB (8%), 260.0 kB/s, 24 min 10 s
    /// left`, the longest open span on its threads with its age, or the stage it is in (`waits
    /// for its pinned native inputs, 1 of 1 layouts left`); `None` while a unit compiles.
    pub on: Option<String>,
    /// The build as it runs now, the one model the view draws ([`blaze_protocol::now::Now`]);
    /// the strings around it say the same for a client that draws no view.
    pub now: blaze_protocol::now::Now,
    /// The daemon's workers running a job's code now (`blaze_graph::pool::Pool::working`),
    /// and those that are not: with most idle, the build waits on what the busy ones run.
    pub busy: u32,
    pub idle: u32,
    /// Each unit under way ([`UnitOf::id`]) and what it is inside now, by the id order: those
    /// a thread works for now. A unit the client has under way and that is not here is
    /// blocked (parked on a wait, queued), not running.
    pub units: Vec<UnitDoing>,
    /// How long the request has been under way, on the daemon's clock: what a client paces a
    /// line off a terminal by.
    pub for_ms: u64,
    /// The innermost open span of the busy worker working for this build longest, with its
    /// age: `codegen of `core::ptr::is_freeze` in core (4 min 30 s)`; `None` with no worker
    /// busy for it.
    pub running: Option<String>,
}

/// One unit under way and what it is inside now ([`Doing::units`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitDoing {
    /// Its [`UnitOf::id`].
    pub id: u32,
    /// `waiting on the download of llvm-….tar.xz (1.2 of 2.0 GB)`, `unit bodies (infer)`,
    /// `build script run 12 s`: the innermost open span of the thread working for it, with
    /// its age.
    pub doing: String,
}

/// What a [`Request::Doc`] documented.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Documented {
    /// Where the pages go, absolute: cargo's `target/doc`, or `target/<triple>/doc` with
    /// `--target`.
    pub dir: String,
    pub pages: u64,
    /// In the order cargo says `Documenting`: the crates asked for first.
    pub crates: Vec<DocumentedCrate>,
    pub source_pages: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct DocumentedCrate {
    /// The crate's name, its directory under [`Documented::dir`].
    pub name: String,
    /// Asked for, not a dependency documented with it: what `Generated` counts.
    pub asked: bool,
    /// Its package as cargo's `Documenting` line names it: `serde v1.0.219`, then
    /// `(/abs/path)` for a path package.
    pub package: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// rustc's error code (`E0308`), or a lint's name.
    pub code: Option<String>,
    /// The unit it is of, for cargo's `compiler-message` and its "generated N warnings".
    pub unit: Option<UnitOf>,
    /// The facts the query that gave it read: what to change to make it go away, and why it
    /// came back after an edit.
    pub facts: Vec<FactRead>,
    pub level: Level,
    pub spans: Vec<Span>,
    pub message: String,
    pub children: Vec<Diagnostic>,
    /// rustc's text for it, as rustc prints it (in color when the request's
    /// [`Cargo::color`], in [`Cargo::width`] columns).
    pub rendered: String,
    /// rustc's long explanation of `code` (`rustc --explain`'s markdown), the `explanation`
    /// of its JSON's `code`, in every format. `None` for a lint, a code with none, and a
    /// child.
    pub explanation: Option<String>,
}

impl Diagnostic {
    /// A note of the whole request, of no unit, span or code: `note: {message}`.
    pub fn note(message: String) -> Diagnostic {
        Diagnostic {
            code: None,
            unit: None,
            facts: Vec::new(), // alloc: none
            level: Level::Note,
            spans: Vec::new(), // alloc: none
            rendered: format!("note: {message}\n"), // alloc: the report owns it, once per note
            message,
            children: Vec::new(), // alloc: none
            explanation: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitOf {
    /// The daemon's index of the unit in the request's unit graph: dense from 0 and the same
    /// in every response of the request, so a client finds a unit's row by it.
    pub id: u32,
    /// The unit is compiled with `--test` (cargo's `CompileMode::Test`): cargo names it
    /// `` `foo` (lib test) `` in "generated N warnings" and "could not compile".
    pub test: bool,
    pub target: UnitTarget,
    /// The unit is a library's doc tests (cargo's `CompileMode::Doctest`), named
    /// `` `foo` (lib doctest) `` in "could not compile", never as the library itself.
    pub doctest: bool,
    pub package_id: String,
    pub manifest_path: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitStarted {
    pub name: String,
    pub verb: Verb,
    /// What cargo prints after the version: `(/abs/path)` for a path package,
    /// `(https://github.com/o/r#0123abcd)` for a git one, none for a registry's.
    pub source: Option<String>,
    /// The daemon's index of the package (`package_id`) in the request's load: dense from 0
    /// and the same in every response of the request.
    pub package: u32,
    pub version: String,
    pub package_id: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Compiling,
    Checking,
    Fresh,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchVerb {
    Updating,
    Downloaded,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct BuildScriptRan {
    pub env: Vec<(String, String)>,
    pub cfgs: Vec<String>,
    pub out_dir: String,
    pub package_id: String,
    pub linked_libs: Vec<String>,
    pub linked_paths: Vec<String>,
}

/// A unit's target as cargo's JSON messages name it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitTarget {
    pub doc: bool,
    pub kind: TargetKind,
    pub name: String,
    pub test: bool,
    pub doctest: bool,
    pub edition: String,
    pub src_path: String,
    /// As cargo's `rustc_crate_types`: a library's (and an example library's) own, in the
    /// manifest's order; `bin` for every other kind.
    pub crate_types: Vec<CrateType>,
}

impl UnitTarget {
    /// cargo's JSON `kind` of it (`TargetKind`'s serialization): a library's crate types, else
    /// its kind's one word.
    pub fn kinds(&self) -> impl Iterator<Item = &'static str> + '_ {
        let (word, types) = match self.kind {
            TargetKind::Lib => (None, &self.crate_types[..]),
            kind => (Some(kind.name()), &[][..]),
        };
        word.into_iter().chain(types.iter().map(|it| it.name()))
    }
}

/// A target's kind, as cargo's `TargetKind` has it; an example library is an
/// [`TargetKind::Example`] whose crate types are not `bin`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum TargetKind {
    Lib,
    Bin,
    Test,
    Bench,
    Example,
    CustomBuild,
}

impl TargetKind {
    pub const ALL: [TargetKind; 6] = [
        TargetKind::Lib,
        TargetKind::Bin,
        TargetKind::Test,
        TargetKind::Bench,
        TargetKind::Example,
        TargetKind::CustomBuild,
    ];

    /// The kind [`TargetKind::name`] names `name`.
    pub fn named(name: &str) -> Option<TargetKind> {
        TargetKind::ALL.into_iter().find(|it| it.name() == name)
    }

    /// cargo's word for it, what its JSON `kind` says of every kind but a library.
    pub const fn name(self) -> &'static str {
        match self {
            TargetKind::Lib => "lib",
            TargetKind::Bin => "bin",
            TargetKind::Test => "test",
            TargetKind::Bench => "bench",
            TargetKind::Example => "example",
            TargetKind::CustomBuild => "custom-build",
        }
    }
}

/// A crate type, as rustc's `--crate-type` names it. cargo passes rustc any other word, which
/// rustc refuses; the daemon refuses it first, naming it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum CrateType {
    Bin,
    Lib,
    Rlib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
    Sdylib,
}

impl CrateType {
    pub const ALL: [CrateType; 8] = [
        CrateType::Bin,
        CrateType::Lib,
        CrateType::Rlib,
        CrateType::Dylib,
        CrateType::Cdylib,
        CrateType::Staticlib,
        CrateType::ProcMacro,
        CrateType::Sdylib,
    ];

    /// The crate type [`CrateType::name`] names `name`.
    pub fn named(name: &str) -> Option<CrateType> {
        CrateType::ALL.into_iter().find(|it| it.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            CrateType::Bin => "bin",
            CrateType::Lib => "lib",
            CrateType::Rlib => "rlib",
            CrateType::Dylib => "dylib",
            CrateType::Cdylib => "cdylib",
            CrateType::Staticlib => "staticlib",
            CrateType::ProcMacro => "proc-macro",
            CrateType::Sdylib => "sdylib",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitProfile {
    pub test: bool,
    pub debuginfo: u32,
    pub opt_level: String,
    pub overflow_checks: bool,
    pub debug_assertions: bool,
}

/// What a build did: the counts the oracle's over-invalidation and the budgets read.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Worked {
    /// [`LinkKind::Placed`] when any of [`Worked::links`] laid its image out whole: one that is
    /// neither [`LinkHow::Spliced`] nor [`LinkHow::Unchanged`].
    pub link: LinkKind,
    /// Units started that were fresh: replayed from the store as they were, nothing made
    /// again (the daemon's `Job::fresh`).
    pub found: u64,
    /// Queries verified green.
    pub green: u64,
    /// Each image the build linked, in the order it linked them.
    pub links: Vec<Linked>,
    /// Queries executed.
    pub reran: u64,
    /// The steps it computed again instead of reusing, each once: the misses `blaze why`
    /// explains and [`Rebuilt::summary`] sums up. 0 says that summary is empty, so the
    /// build's client asks for none ([`Request::Rebuilt`] with `build`).
    pub misses: u64,
    /// How the files of the outputs it placed in the build dir got there.
    pub placed: PlacedBytes,
    /// Each crate of the build that is not a workspace member, sorted by cargo package name:
    /// whether its check outcome and its bodies were served.
    pub served: Vec<ServedCrate>,
    /// The build's id in the daemon's profile history (`builds.build_id`): what the client
    /// tags the rows it sends with ([`Request::ClientRows`]), what `build-finished` carries
    /// and what `blaze slow --build N` and `blaze debug sql` take. `None` for work no build
    /// row stands for.
    pub build_id: Option<u64>,
    /// Units started that were made again (the daemon's `Job::compiled`).
    pub compiled: u64,
    /// The measured run time of the request's nodes, summed by kind: each kind that ran at
    /// all, in [`NodeKind::ALL`]'s order.
    pub steps_us: Vec<(NodeKind, u64)>,
    /// The plane's critical path of the request.
    pub critical_path_us: u64,
}

/// What a build left unchecked ([`Response::Unchecked`]): said once a unit failed and any
/// unit was skipped for it, or checked against a unit that failed open. Each list in the
/// plan's order of its units, every unit named as blaze's spans name it:
/// `libc v0.2.155 (build script run)`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Unchecked {
    /// Each unit that failed only in its bodies or lints, its items sound: under
    /// keep-going (the default) what depends on it was checked against its items, while it stays failed.
    pub open: Vec<OpenFailure>,
    /// Each test executable or doc test unit that never built, with the failure that held it
    /// back (its own, or the unit it was skipped behind), in the plan's order: what the end of
    /// `blaze test` lists as never run.
    pub tests: Vec<Unbuilt>,
    /// Each failure units were not checked behind.
    pub behind: Vec<Behind>,
    /// The build stopped at its first failure (`--fail-fast`): the units [`Unchecked::behind`]
    /// names under it were skipped for that, whether or not they depend on it.
    pub stopped: bool,
}

/// A test unit of [`Unchecked::tests`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Unbuilt {
    /// Why that one failed, as its node says.
    pub why: String,
    /// The test unit, as blaze's spans name it: `calc v0.1.0 (test "it")`.
    pub unit: String,
    /// The unit whose failure held it back: itself when its own compile failed.
    pub behind: String,
}

/// A failure of [`Unchecked::behind`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Behind {
    /// The dense id ([`UnitOf::id`]) of the unit that failed, where the daemon knows it: what a
    /// [`Response::Report`] of that unit is matched to its blocked units by.
    pub id: Option<u32>,
    /// Why it failed, as its node says: `the build script failed`, `errors in its items`.
    pub why: String,
    /// The unit that failed.
    pub unit: String,
    /// Each unit not checked behind it.
    pub units: Vec<String>,
}

/// A unit of [`Unchecked::open`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct OpenFailure {
    pub unit: String,
    /// Each unit that was checked with it among its dependencies.
    pub checked: Vec<String>,
}

/// How a build put its outputs' files into its build dir, in bytes (docs/cargo.md, "Where a
/// build writes"): a copy where a clone was meant shows here.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PlacedBytes {
    /// Cloned from the store, sharing its blocks: no new bytes on disk.
    pub cloned: u64,
    /// Copied out of the store by writing: outputs too short to clone.
    pub copied: u64,
    /// Written into the store once, as the whole files later builds clone.
    pub written: u64,
}

/// A dependency crate of [`Worked::served`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ServedCrate {
    /// The `Display` of `blaze_diagnostics::cache::Refused` or `hir_def::deps::serve::Refused`
    /// when either part is [`ServedPart::Refused`], else `None`.
    pub why: Option<String>,
    pub bodies: ServedPart,
    pub outcome: ServedPart,
    /// cargo's package name, not the crate name: the one that is unique in a build.
    pub package: String,
}

/// What became of a part of a dependency crate's check.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum ServedPart {
    /// Read from the store and used.
    Served,
    /// Not in the store: checked this session and offered to the store.
    Checked,
    /// Neither served nor stored.
    Refused,
}

impl ServedPart {
    pub const ALL: [ServedPart; 3] = [ServedPart::Served, ServedPart::Checked, ServedPart::Refused];

    /// The part [`ServedPart::name`] names `name`.
    pub fn named(name: &str) -> Option<ServedPart> {
        ServedPart::ALL.into_iter().find(|it| it.name() == name)
    }

    /// The word of the `blaze-served` line.
    pub const fn name(self) -> &'static str {
        match self {
            ServedPart::Served => "served",
            ServedPart::Checked => "checked",
            ServedPart::Refused => "refused",
        }
    }
}

/// One image of [`Worked::links`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Linked {
    pub how: LinkHow,
    pub path: String,
}

/// How an image was linked.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum LinkHow {
    /// No image of it was kept: laid out whole.
    First,
    /// What changed was placed into the room the kept image keeps.
    Spliced,
    /// Nothing it is given changed since the kept image's last link (a doctest example beside
    /// an edited one): nothing was compiled, staged or written, and the file is the kept
    /// image's, byte for byte.
    Unchanged,
    /// Laid out whole although an image was kept: the names of the output's fields that differ
    /// from the kept image's (`exports`, `debug`), `not incremental` for an output that keeps
    /// no room, `layout` for a changed layout profile, or `no thunk in reach` for a splice
    /// refused because a branch in code kept in place could reach no thunk.
    Full { why: Vec<String> },
}

/// The `link:` line's words: `first link`, `spliced`, `unchanged`, `not spliced: <why>, <why>`.
impl std::fmt::Display for LinkHow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkHow::First => f.write_str("first link"),
            LinkHow::Spliced => f.write_str("spliced"),
            LinkHow::Unchanged => f.write_str("unchanged"),
            LinkHow::Full { why } => {
                f.write_str("not spliced: ")?;
                for (at, it) in why.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(it)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkKind {
    /// The image was patched in place.
    Patched,
    /// The image was placed whole.
    Placed,
}

/// One result of a view's last build ([`Request::Trace`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Traced {
    pub key: QueryKey,
    /// The build ran it, rather than keeping it.
    pub ran: bool,
    /// In read order.
    pub edges: Vec<blaze_query::Edge>,
    pub fingerprint: Fingerprint,
}

/// One of the daemon's long-lived tables ([`Request::Tables`]): `token`, `syntax-node`,
/// `hir-expr`, `mir-statement`, `memo`, `memo-edge`, `relocation` and the rest.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Table {
    pub name: String,
    /// All its columns and side tables.
    pub bytes: u64,
    pub elements: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
    /// rustc's closing lines ("For more information about this error, try `rustc --explain
    /// E0308`."): printed with no level before them, `"failure-note"` in its JSON.
    FailureNote,
}

impl Level {
    /// As rustc's JSON and its rendered lines name it.
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Note => "note",
            Level::Help => "help",
            Level::FailureNote => "failure-note",
        }
    }
}

/// A place in a file of the view, by 1-based line and column of characters.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub file: String,
    pub line: u32,
    pub label: Option<String>,
    pub column: u32,
    pub primary: bool,
    pub byte_end: u32,
    pub end_line: u32,
    pub byte_start: u32,
    pub end_column: u32,
    pub replacement: Option<String>,
    pub applicability: Option<Applicability>,
}

/// How sure a suggestion is, as rustc's JSON `suggestion_applicability` names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

impl Applicability {
    pub const ALL: [Applicability; 4] = [
        Applicability::MachineApplicable,
        Applicability::MaybeIncorrect,
        Applicability::HasPlaceholders,
        Applicability::Unspecified,
    ];

    /// The applicability [`Applicability::name`] names `name`.
    pub fn named(name: &str) -> Option<Applicability> {
        Applicability::ALL.into_iter().find(|it| it.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Applicability::MachineApplicable => "MachineApplicable",
            Applicability::MaybeIncorrect => "MaybeIncorrect",
            Applicability::HasPlaceholders => "HasPlaceholders",
            Applicability::Unspecified => "Unspecified",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Artifact {
    /// The variables cargo sets for running it (`Compilation::target_process`, `fill_env`):
    /// `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `CARGO_PKG_*`, `CARGO_CRATE_NAME`... ,
    /// `OUT_DIR` of a package with a build script, the library search path
    /// (`DYLD_FALLBACK_LIBRARY_PATH`/`LD_LIBRARY_PATH`/`PATH`) with cargo's directories first.
    /// None of the user's environment. Empty for an artifact that does not run.
    pub env: Vec<(String, String)>,
    pub kind: ArtifactKind,
    /// The main file placed (the executable for one that runs), and its content.
    pub path: String,
    pub fresh: bool,
    /// What runs the executable before it, cargo's `target.<triple>.runner` (and
    /// `CARGO_TARGET_<TRIPLE>_RUNNER`): program then arguments; empty for none.
    pub runner: Vec<String>,
    pub target: UnitTarget,
    pub content: ContentId,
    /// The target's `harness` (`[[test]] harness = false` is false): cargo passes `--quiet`
    /// to a harness under `-q`.
    pub harness: bool,
    /// cargo's package id (`path+file:///w/a#0.1.0`).
    pub package: String,
    pub profile: UnitProfile,
    pub features: Vec<String>,
    /// Every file placed, as cargo's `filenames`.
    pub filenames: Vec<String>,
    pub executable: Option<String>,
    pub manifest_path: String,
}

/// What an [`Artifact`] is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum ArtifactKind {
    Bin,
    Lib,
    Test,
    Example,
    Bench,
    ProcMacro,
    CustomBuild,
    Doctest,
}

impl ArtifactKind {
    pub const ALL: [ArtifactKind; 8] = [
        ArtifactKind::Bin,
        ArtifactKind::Lib,
        ArtifactKind::Test,
        ArtifactKind::Example,
        ArtifactKind::Bench,
        ArtifactKind::ProcMacro,
        ArtifactKind::CustomBuild,
        ArtifactKind::Doctest,
    ];

    /// The kind [`ArtifactKind::name`] names `name`.
    pub fn named(name: &str) -> Option<ArtifactKind> {
        ArtifactKind::ALL.into_iter().find(|it| it.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            ArtifactKind::Bin => "bin",
            ArtifactKind::Lib => "lib",
            ArtifactKind::Test => "test",
            ArtifactKind::Example => "example",
            ArtifactKind::Bench => "bench",
            ArtifactKind::ProcMacro => "proc-macro",
            ArtifactKind::CustomBuild => "custom-build",
            ArtifactKind::Doctest => "doctest",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TestResult {
    pub name: String,
    /// The test executable, as cargo's `Running` line names it.
    pub binary: String,
    /// Not run: the result that stands, and what it was kept by. A cached result is
    /// `Unreached` with `millis` 0.
    pub cached: Option<Cached>,
    pub millis: u64,
    #[serde(with = "blaze_protocol::bytes")]
    pub output: Vec<u8>,
    pub outcome: TestOutcome,
    /// It ran outside the sandbox (docs/cargo.md), so the command's summary names it. False for
    /// a cached result, whose run is not this one.
    pub outside: bool,
    /// The daemon's index of `binary` among the request's test executables: dense from 0, the
    /// same for every result of one executable, so a client groups results by it.
    pub executable: u32,
}

/// The run that stands for a test not run.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Cached {
    pub basis: Basis,
    /// How long the run that stands took.
    pub millis: u64,
    pub outcome: TestOutcome,
}

/// What a cached result was kept by.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Basis {
    /// The functions the run executed.
    Executed,
    /// What the test reaches, as its run's record was not whole, and why.
    Reached { why: String },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestOutcome {
    Passed,
    Failed,
    Ignored,
    /// Not run: the edit reached none of its code; its last result stands.
    Unreached,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Status {
    /// The daemon's process: what `kill` and the restart budgets name.
    pub pid: u32,
    /// The bugs the daemon found in itself and went on past so far, one a site
    /// (`stdx::assert::reported`): empty in a healthy daemon.
    pub bugs: Vec<Bug>,
    /// Which blaze the daemon is: a client of another build never sends it a request of one.
    pub build: Build,
    pub views: u32,
    /// Every build under way, oldest first, as of the daemon's last look (once a second while
    /// one runs): what it is, its stage, and what its threads are inside.
    pub builds: Vec<Building>,
    /// The profile history's last error (`blaze_profile::history`): a flush that failed,
    /// naming the file; `None` while every flush has been written.
    pub history: Option<String>,
    pub requests: Vec<u64>,
    /// What the daemon was started with that it keeps for its life
    /// (`blaze-cargo`'s `serve::settings::STARTED_WITH`, one each, in that order): a client
    /// whose own environment says otherwise restarts an idle daemon, or refuses at once, before
    /// it sends a request, never builds under the daemon's values.
    pub settings: Vec<Setting>,
    /// Every download in flight in the daemon (`blaze_fetch::downloads`), oldest first.
    pub downloads: Vec<Downloading>,
    /// How long the daemon has run, on its driver's clock (which starts with it), as of this
    /// answer.
    pub uptime_ms: u64,
    pub budget_bytes: u64,
    pub memory_bytes: u64,
}

/// One build under way ([`Status::builds`]). Its fields are declared in the order CBOR writes
/// a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Building {
    /// Its plane request: the build's number in the profile history and its spans; 0 before
    /// the job has posted one (while it fetches, or its views open).
    pub id: u32,
    /// Its last live model ([`Doing::now`]): what `blaze status` draws.
    pub now: blaze_protocol::now::Now,
    /// The command, as the client ran it: `test -p blaze-api`, or the work's name for an
    /// agent's request (`check`).
    pub what: String,
    /// Where it is: its stage and what it waits on there (`waits for its pinned native
    /// inputs, 1 of 1 layouts left`), or the units it builds and how many run.
    pub phase: String,
    /// Each thread working for it, a line each: its outermost open span and its innermost,
    /// with their ages (`blaze-worker-3: task `unit bodies (infer)` (2 min 05 s) > infer_query
    /// `app::main` (1 s)`); at most a few, the longest open first.
    pub spans: Vec<String>,
    /// How long it has been under way, on the daemon's clock.
    pub for_ms: u64,
}

/// One download in flight ([`Status::downloads`]), as `blaze_fetch::progress::Line` counts
/// it. Its fields are declared in the order CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Downloading {
    /// Where from, redacted.
    pub url: String,
    pub done: u64,
    /// Bytes a second over the last few seconds; `None` before there are two samples.
    pub rate: Option<u64>,
    /// What it is: the URL's file name, or what the downloader called it.
    pub what: String,
    pub total: Option<u64>,
    /// How long since its last byte.
    pub idle_ms: u64,
    /// How long since it began.
    pub elapsed_ms: u64,
}

/// One setting a daemon takes from how it was started and keeps for its life
/// ([`Status::settings`]): an environment variable's name and the value the daemon runs with,
/// `None` where it was unset. Never a secret: no token or key is one.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Setting {
    pub name: String,
    pub value: Option<String>,
}

/// One site's bugs the daemon reported and went on past ([`Status::bugs`]): a value formatted
/// with no database attached, a `never!` that failed. Its fields are declared in the order
/// CBOR writes a map's keys: shorter first, then bytewise.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Bug {
    /// The site that reported it: `file:line:column`.
    pub at: String,
    /// The kind of value or check it is of: `AdtDef`, `assertion` (a `never!` or `always!`).
    pub kind: String,
    /// What went wrong: "formatted with no database attached".
    pub what: String,
    pub count: u64,
    /// The thread of its first report, as named.
    pub thread: Option<String>,
}

/// "a blaze bug: `AdtDef` formatted with no database attached at <file:line:col>, 2 times,
/// first on thread `blaze-worker-3`", as `stdx::assert::reported::Site` says it.
impl std::fmt::Display for Bug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Bug { kind, what, at, count, thread } = self;
        write!(
            f,
            "a blaze bug: `{kind}` {what} at {at}, {count} time{}",
            if *count == 1 { "" } else { "s" }
        )?;
        match thread {
            Some(thread) => write!(f, ", first on thread `{thread}`"),
            None => Ok(()),
        }
    }
}

/// Which blaze a daemon is ([`Status::build`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Build {
    /// The hash of its executable's bytes as it started: two builds of one commit, a dirty
    /// tree's, are two.
    pub id: ContentId,
    /// Its executable as it was when the daemon started: a client whose own is this file,
    /// unchanged (the same length, modification time and inode), is the same build without
    /// hashing itself.
    pub exe: Executable,
}

/// A file's path and what changes when its bytes do: its length, modification time and inode.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Executable {
    pub len: u64,
    pub path: String,
    pub inode: u64,
    pub modified_ns: u64,
}

/// The daemon's memory by subsystem ([`Request::Memory`]): blaze-mem's counters
/// (`blaze_mem::tag`), read with relaxed loads, so each number is exact and the set is not
/// one instant.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Memory {
    /// One per `blaze_mem::Tag`, in its order.
    pub tags: Vec<TagMemory>,
    /// Bytes committed in every region of blaze-mem's now, whatever holds them: with the
    /// global allocator's, what admission counts as blaze's own.
    pub committed: u64,
    pub committed_high_water: u64,
}

/// One subsystem's memory: what it holds now and at most, what it handed out in all, the
/// rate it hands out at, the most one task of it took from its frame, and the most it should
/// hold.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TagMemory {
    /// The tag's name: `parse`, `infer`, `codegen`, `io` and the rest.
    pub tag: String,
    pub live: u64,
    /// Bytes a second handed out since the last [`Request::Memory`] of the daemon; 0 for the
    /// first.
    pub rate: u64,
    /// `u64::MAX` for no budget.
    pub budget: u64,
    pub allocated: u64,
    pub task_peak: u64,
    pub high_water: u64,
}

/// What failed, and where: a [`Report`] (docs/v2/plan/design/structured-output.md), which is all
/// the wire carries. `what` and `at` are its words, flat ([`Report::flat`]), for a caller that
/// puts an error into a sentence of its own; they are made from the report and never change
/// it, so an error is changed through its report ([`ApiError::and`], [`ApiError::from`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ApiError {
    /// A file, a package, a query, a function: where it failed, as precisely as known.
    pub at: Option<String>,
    pub what: String,
    report: Report,
}

impl ApiError {
    /// An error that is words only ([`Report::text`]): a caller not yet given a report of its
    /// own. docs/v2/plan/design/structured-output.md counts these down.
    pub fn new(what: impl Into<String>, at: Option<String>) -> ApiError {
        ApiError::from(Report::text(what, at))
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    pub fn into_report(self) -> Report {
        self.report
    }

    /// The error with `more` said after its headline: `what; more`.
    pub fn and(self, more: &str) -> ApiError {
        let mut report = self.report;
        report.headline.push(blaze_report::Span::Plain(format!("; {more}"))); // alloc: an error's, once
        ApiError::from(report)
    }
}

impl From<Report> for ApiError {
    fn from(report: Report) -> ApiError {
        ApiError { at: report.at.as_ref().map(Text::flat), what: report.flat(), report }
    }
}

impl From<ApiError> for Report {
    fn from(error: ApiError) -> Report {
        error.report
    }
}

impl Serialize for ApiError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.report.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ApiError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<ApiError, D::Error> {
        Report::deserialize(deserializer).map(ApiError::from)
    }
}

/// A fact a result read (`blaze_items::Fact::key`), as a diagnostic, a reach or a mismatch
/// names it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FactRead {
    pub key: QueryKey,
    /// What it stands for, for people and LLMs: "the signature of `parser::Token::new`".
    pub what: String,
}

/// What a change from one view to another reaches, building only the proc macros and build
/// scripts' runs each view's front end applies.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Reach {
    /// The items whose hashes changed, each with the facts of it that changed.
    pub items: Vec<ReachedItem>,
    /// The tests whose reached code changed, which a test request would run.
    pub tests: Vec<String>,
    pub changes: Vec<Change>,
    /// The functions that would be compiled again, by path.
    pub compiled: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ReachedItem {
    /// The item's path, as rustc prints it.
    pub path: String,
    pub facts: Vec<FactRead>,
}

/// One candidate of a best-of-N, as it was ranked: fewest errors, then fewest failed tests,
/// then most passed, then fewest warnings, then its place in the request; a candidate whose
/// ops did not apply or whose job could not run goes after every one that ran.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Ranked {
    /// The candidate's request ended with `ok` true.
    pub ok: bool,
    /// The base's root for a candidate whose ops did not apply.
    pub root: ContentId,
    pub errors: u32,
    pub failed: u32,
    pub passed: u32,
    pub warnings: u32,
    pub candidate: u32,
}

/// A result the database held that a build from nothing made otherwise: the oracle's finding.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Mismatch {
    /// The fact whose edge should have changed and did not, where the oracle found it.
    pub fact: Option<FactRead>,
    pub query: QueryKey,
    pub scratch: Fingerprint,
    pub incremental: Fingerprint,
}

/// A request of a connection: what the server and the session name a reply's destination by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RequestRef {
    pub conn: Conn,
    pub id: u64,
}

/// What the server hands the session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Arrived {
    Request {
        from: RequestRef,
        request: Request,
    },
    /// The connection ended: its requests are cancelled and its replies dropped.
    Gone {
        conn: Conn,
    },
}

/// A response for the server to send.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    pub to: RequestRef,
    pub response: Response,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names blaze-cargo's `cli/json.rs` and blaze-ci's `service.rs` each wrote.
    #[test]
    fn levels_are_named_as_rustc_names_them() {
        let all = [Level::Error, Level::Warning, Level::Note, Level::Help, Level::FailureNote];
        let names = all.map(Level::name);
        assert_eq!(names, ["error", "warning", "note", "help", "failure-note"]);
    }

    #[test]
    fn a_request_round_trips_in_deterministic_cbor() {
        let request = Envelope::new(7, Request::Cancel { id: 3 });
        let bytes = blaze_protocol::cbor::to_vec(&request).unwrap();
        let back: Envelope<Request> = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(back, request);
        assert_eq!(back.schema, SCHEMA);
    }

    /// A run's output is one byte string; JSON shows it as the array of numbers it was.
    #[test]
    fn bytes_go_out_as_byte_strings() {
        let ran = Response::Ran { code: Some(0), stderr: vec![1], stdout: b"hi".to_vec() };
        let bytes = blaze_protocol::cbor::to_vec(&ran).unwrap();
        let mut want = vec![0xa1, 0x63, b'R', b'a', b'n', 0xa3, 0x64, b'c', b'o', b'd', b'e', 0x00];
        want.extend_from_slice(&[0x66, b's', b't', b'd', b'e', b'r', b'r', 0x41, 1]);
        want.extend_from_slice(&[0x66, b's', b't', b'd', b'o', b'u', b't', 0x42, b'h', b'i']);
        assert_eq!(bytes, want);
        assert_eq!(blaze_protocol::cbor::from_slice::<Response>(&bytes).unwrap(), ran);
        let json = serde_json::to_string(&ran).unwrap();
        assert_eq!(json, r#"{"Ran":{"code":0,"stderr":[1],"stdout":[104,105]}}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), ran);

        let result = TestResult {
            name: "t".to_owned(),
            binary: "b".to_owned(),
            cached: None,
            millis: 1,
            output: vec![9; 30],
            outcome: TestOutcome::Passed,
            outside: true,
            executable: 0,
        };
        let bytes = blaze_protocol::cbor::to_vec(&result).unwrap();
        let mut output = vec![0x66, b'o', b'u', b't', b'p', b'u', b't', 0x58, 30];
        output.extend_from_slice(&[9; 30]);
        assert!(bytes.windows(output.len()).any(|it| it == output), "{bytes:02x?}");
        assert_eq!(blaze_protocol::cbor::from_slice::<TestResult>(&bytes).unwrap(), result);

        let cargo = Cargo {
            cwd: "/w".to_owned(),
            env: Vec::new(),
            args: Vec::new(),
            root: "/w".to_owned(),
            color: false,
            files: vec![("/c".to_owned(), vec![1, 2])],
            width: Some(100),
            command: "build".to_owned(),
            verbose: 2,
            refused: None,
        };
        let bytes = blaze_protocol::cbor::to_vec(&cargo).unwrap();
        let files = [0x65, b'f', b'i', b'l', b'e', b's', 0x81, 0x82, 0x62, b'/', b'c', 0x42, 1, 2];
        assert!(bytes.windows(files.len()).any(|it| it == files), "{bytes:02x?}");
        let width = [0x65, b'w', b'i', b'd', b't', b'h', 0x18, 100];
        assert!(bytes.windows(width.len()).any(|it| it == width), "{bytes:02x?}");
        assert_eq!(blaze_protocol::cbor::from_slice::<Cargo>(&bytes).unwrap(), cargo);
        let json = serde_json::to_value(&cargo).unwrap();
        assert_eq!(json["files"], serde_json::json!([["/c", [1, 2]]]));
    }

    /// A fixed name set goes out by name, as the strings it was went; a name outside it is
    /// refused, naming it.
    #[test]
    fn a_fixed_name_is_written_by_name_and_an_unknown_one_refused() {
        let names = [
            (blaze_protocol::cbor::to_vec(&TargetKind::CustomBuild).unwrap(), "custom-build"),
            (blaze_protocol::cbor::to_vec(&CrateType::ProcMacro).unwrap(), "proc-macro"),
            (blaze_protocol::cbor::to_vec(&ArtifactKind::Doctest).unwrap(), "doctest"),
            (
                blaze_protocol::cbor::to_vec(&Applicability::MachineApplicable).unwrap(),
                "MachineApplicable",
            ),
            (blaze_protocol::cbor::to_vec(&TargetKind::Lib).unwrap(), TargetKind::Lib.name()),
        ];
        for (bytes, name) in names {
            assert_eq!(bytes, blaze_protocol::cbor::to_vec(name).unwrap(), "{name}");
        }
        fn each<T: Serialize + serde::de::DeserializeOwned + Copy + PartialEq + std::fmt::Debug>(
            all: &[T],
            name: fn(T) -> &'static str,
            named: fn(&str) -> Option<T>,
        ) {
            for &it in all {
                let bytes = blaze_protocol::cbor::to_vec(name(it)).unwrap();
                assert_eq!(blaze_protocol::cbor::to_vec(&it).unwrap(), bytes, "{it:?}");
                assert_eq!(blaze_protocol::cbor::from_slice::<T>(&bytes), Ok(it));
                assert_eq!(named(name(it)), Some(it));
            }
            assert_eq!(named("staticlibs"), None);
        }
        each(&TargetKind::ALL, TargetKind::name, TargetKind::named);
        each(&CrateType::ALL, CrateType::name, CrateType::named);
        each(&ArtifactKind::ALL, ArtifactKind::name, ArtifactKind::named);
        each(&Applicability::ALL, Applicability::name, Applicability::named);
        each(&ServedPart::ALL, ServedPart::name, ServedPart::named);
        let bytes = blaze_protocol::cbor::to_vec("staticlibs").unwrap();
        let refused = blaze_protocol::cbor::from_slice::<CrateType>(&bytes).unwrap_err();
        assert!(refused.to_string().contains("`staticlibs`"), "{refused}");
        let item = blaze_protocol::cbor::to_vec(&Envelope::new(
            3,
            Response::Fetched { verb: FetchVerb::Updating, what: String::new() },
        ))
        .unwrap();
        let updating = [0x68, b'U', b'p', b'd', b'a', b't', b'i', b'n', b'g'];
        let at = item.windows(updating.len()).position(|it| it == updating).unwrap();
        let mut unknown = item.clone();
        unknown[at + 1] = b'X';
        let refused = frames::decode::<Response>(&unknown).unwrap_err();
        assert_eq!(refused.id, Some(3));
        assert!(refused.error.what.contains("`Xpdating`"), "{refused:?}");
        let refused = frames::decode::<ResponseRef<'_>>(&unknown).unwrap_err();
        assert!(refused.error.what.contains("`Xpdating`"), "{refused:?}");
    }

    /// cargo's JSON `kind`: a library's crate types, any other target's one word.
    #[test]
    fn a_targets_kinds_are_cargos() {
        let target = |kind, crate_types| UnitTarget {
            doc: false,
            kind,
            name: "a".to_owned(),
            test: false,
            doctest: false,
            edition: "2024".to_owned(),
            src_path: "/w/src/lib.rs".to_owned(),
            crate_types,
        };
        let lib = target(TargetKind::Lib, vec![CrateType::Rlib, CrateType::Cdylib]);
        assert_eq!(lib.kinds().collect::<Vec<_>>(), ["rlib", "cdylib"]);
        let example = target(TargetKind::Example, vec![CrateType::Cdylib]);
        assert_eq!(example.kinds().collect::<Vec<_>>(), ["example"]);
        let script = target(TargetKind::CustomBuild, vec![CrateType::Bin]);
        assert_eq!(script.kinds().collect::<Vec<_>>(), ["custom-build"]);
    }
}
