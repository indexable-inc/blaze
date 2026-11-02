//! The agent API (docs/v2/plan/lanes/ci.md, docs/v2/plan.md section 1; #277; rule no-mcp): the daemon's
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

pub use borrowed::{
    ApiErrorRef, BasisRef, BuildScriptRanRef, CachedRef, DiagnosticRef, FactReadRef, LiveFrameRef, ResponseRef,
    SpanRef, TestResultRef, UnitOfRef, UnitStartedRef, UnitTargetRef,
};
pub use blaze_protocol::events::NodeKind;
pub use intent::{Edit, Edited, EditedItem, How, Target};
pub use blaze_vfs::{Mode, Op};

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
/// name as before. [`ResponseRef`] reads the same bytes. 15: [`Request::Pack`] and
/// [`Response::Packed`]. 16: [`Worked::links`]. 17: [`Worked::steps_us`]. 18: [`Request::Doc`],
/// [`Response::DocPage`] and [`Response::Documented`]. 19: [`Cargo::refused`]. 20:
/// [`Request::Explore`], [`Request::Profile`], [`Request::Live`], [`Request::Sql`], their
/// answers, and [`Status::history`]. 21: [`Edit::Replace`]. 22: [`Status::build`]. 23:
/// [`Request::Hot`] and [`Response::Hot`].
pub const SCHEMA: u32 = 23;

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
    Commit { ops: Vec<Op>, base: ContentId, head: String },
    /// Checks the view; answered with diagnostics.
    Check { view: ViewRef, targets: Targets },
    Build { view: ViewRef, targets: Targets },
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
    Run { bin: Option<String>, args: Vec<String>, view: ViewRef, targets: Targets },
    /// The files that differ between two views.
    Diff { to: ViewRef, from: ViewRef },
    /// Why a query reran in the last build of a view.
    Why { view: ViewRef, query: QueryKey },
    /// What the daemon is doing: its requests, memory, views.
    Status,
    /// Cancels the request `id` of this connection.
    Cancel { id: u64 },
    /// Resolves a view to its root and keeps it open until [`Request::Close`] or the
    /// connection ends; answered with [`Response::Opened`].
    Open { view: ViewRef },
    Close { root: ContentId },
    /// What the change from `from` to `to` reaches (items, facts, functions compiled again,
    /// tests), building nothing; answered with [`Response::Reached`].
    Reach { to: ViewRef, from: ViewRef, targets: Targets },
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
    Verify { view: ViewRef, targets: Targets },
    /// The daemon's state made durable, before the VM snapshots itself; answered with
    /// [`Response::Snapshotted`] once it is.
    Snapshot,
    /// Every instance the view's last build compiled or found, with the hash its code was
    /// made from; answered with [`Response::CodegenInputs`].
    CodegenInputs { view: ViewRef, targets: Targets },
    /// Every result the view's last build ran or kept, with its edges; answered with
    /// [`Response::Trace`].
    Trace { view: ViewRef, targets: Targets },
    /// The size of each of the daemon's long-lived tables; answered with [`Response::Tables`].
    Tables { view: ViewRef },
    /// `blaze stop`: the daemon's state made durable, then it exits; answered with
    /// `Done { ok: true }` before it does.
    Stop,
    /// Edits by meaning, applied in the front end to `base` and committed to `head` as
    /// [`Request::Commit`] commits: answered with [`Response::Edited`], then
    /// [`Response::Committed`] once durable.
    Edit { base: ContentId, head: String, edits: Vec<Edit> },
    /// Declared intent: the tests `filter` selects in the view will be run soon. What they
    /// reach is built ahead at a priority below every request's, answered at once with
    /// [`Response::Announced`]. A request on the same root takes the work over; a newer intent
    /// of this connection, the connection's end, or `wait_ms` with no such request cancels it.
    WillTest { view: ViewRef, filter: Vec<String>, targets: Targets, wait_ms: u64 },
    /// Declared intent: `ops` will be committed to `head`. The what-if is checked (tested,
    /// with `test`) ahead as [`Request::WillTest`] builds ahead; a commit to `head` that makes
    /// the same root takes the work over, and one that makes another root cancels it.
    WillSave { ops: Vec<Op>, head: String, test: bool, targets: Targets, wait_ms: u64 },
    /// What each subsystem's memory is doing, from blaze-mem's counters; answered with
    /// [`Response::Memory`].
    Memory,
    /// The front end of package `program`'s crate and every crate it is made of, checked as
    /// `targets` asks (its build scripts run and its proc macros loaded in the sandbox, every
    /// expansion of the program recorded) as one file for blaze's in-browser compiler
    /// (`blaze_wasm_pack::Pack`); answered with [`Response::Packed`].
    Pack { view: ViewRef, program: String, targets: Targets },
    /// `cargo doc`: the targets it documents checked as `targets` asks (`Targets::cargo`'s
    /// command `doc`), then the HTML documentation of those crates and, unless `no_deps`, of
    /// every library they depend on, written by blaze from the front end (`blaze-doc`), no
    /// rustdoc run. `private` is `--document-private-items`. Answered with
    /// [`Response::Documented`], then each file as a [`Response::DocPage`], which the client
    /// writes, then rustdoc's warnings as [`Response::Diagnostic`]s.
    Doc { view: ViewRef, no_deps: bool, private: bool, targets: Targets },
    /// `blaze explore`: the explorer's server started in the daemon if it is not yet (with
    /// `dev`, also accepting the page's development server), answered with
    /// [`Response::Explored`]. Answered by the daemon's loop, which holds the server.
    Explore { dev: bool },
    /// `blaze debug profile`: a Chrome trace of the last `seconds` of every thread's flight
    /// recorder and the CPU samples; answered with [`Response::Profiled`].
    Profile { seconds: u32 },
    /// `blaze debug live`: a [`Response::Live`] every 250 ms until the connection ends. A
    /// subscriber whose connection has a frame not yet sent is skipped that tick; the next
    /// frame has every row since its last. Answered by the daemon's loop, which holds the feeds.
    Live,
    /// `blaze debug sql --live`: `sql` over the history and the last `last_s` seconds of the
    /// daemon's flight recorder, run by the daemon (the recorder is its); answered with
    /// [`Response::Queried`].
    Sql { sql: String, last_s: u32 },
    /// `blaze debug hot`: the `top` keys (a span and what it is of) that took most time and
    /// each kind of wait's quantiles, from the daemon's live sketches (blaze-profile's `hot`),
    /// no history read; answered with [`Response::Hot`].
    Hot { top: u32, since: HotSince },
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

/// One frame of [`Request::Live`]: blaze-profile's `live::Frame`, each table one Arrow IPC
/// stream of the schema's table of that name.
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
    Committed { root: ContentId, rebased: bool },
    /// A diagnostic, as rustc would give it, with its code.
    Diagnostic(Diagnostic),
    /// A build's output, placed. `profile` as cargo's `Finished` line names it:
    /// "`dev` profile [unoptimized + debuginfo]". `members` is the workspace's member count:
    /// cargo's `needs_spec_flag` puts `-p foo` in "to rerun pass" when there is more than one.
    Built { members: u32, profile: String, artifacts: Vec<Artifact> },
    /// A unit began: cargo's `Compiling foo v0.1.0 (/path)` line.
    Unit(UnitStarted),
    /// A unit's compile ended, after every diagnostic of it: where cargo prints its
    /// "generated N warnings" or "could not compile".
    UnitEnded(UnitOf),
    /// cargo's `Updating crates.io index` / `Downloaded serde v1.0.219` as the daemon fetches
    /// what the load asked for: once for each source a job fetches, once for each index or
    /// git repository it updates.
    Fetched { verb: FetchVerb, what: String },
    /// A build script ran: cargo's `build-script-executed` message.
    BuildScript(BuildScriptRan),
    /// What a build, test or run did, before its [`Response::Done`].
    Work(Worked),
    TestResult(TestResult),
    Ran {
        code: Option<i32>,
        #[serde(with = "blaze_protocol::bytes")]
        stderr: Vec<u8>,
        #[serde(with = "blaze_protocol::bytes")]
        stdout: Vec<u8>,
    },
    Diff { changes: Vec<Change> },
    Why(Why),
    Status(Status),
    /// The request is over; `ok` false after an error or a failed build.
    Done { ok: bool },
    Error(ApiError),
    Opened { root: ContentId },
    Reached(Reach),
    /// A response of candidate `candidate` of a best-of-N.
    Of { response: Box<Response>, candidate: u32 },
    /// Best first.
    Ranked(Vec<Ranked>),
    Verified { mismatches: Vec<Mismatch> },
    /// Every head's root, as made durable.
    Snapshotted { heads: Vec<(String, ContentId)> },
    /// Each instance's key (`blaze_codegen::InstanceKey`) with the code hash
    /// (`blaze_items::CodeHash`) it was made from, sorted by key.
    CodegenInputs(Vec<(ContentId, ContentId)>),
    Trace(Vec<Traced>),
    Tables(Vec<Table>),
    /// What a [`Request::Edit`] changed, before its commit's [`Response::Committed`].
    Edited(Edited),
    /// The root a [`Request::WillTest`] or [`Request::WillSave`] builds ahead.
    Announced { root: ContentId },
    Memory(Memory),
    /// A [`Request::Pack`]'s pack: `blaze_wasm_pack::Pack` as JSON.
    Packed {
        #[serde(with = "blaze_protocol::bytes")]
        pack: Vec<u8>,
    },
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
    Explored { link: String },
    /// [`Request::Profile`]'s Chrome trace (ui.perfetto.dev opens it).
    Profiled {
        #[serde(with = "blaze_protocol::bytes")]
        trace: Vec<u8>,
    },
    Live(LiveFrame),
    /// [`Request::Sql`]'s answer: one Arrow IPC stream of its batches, and its rows.
    Queried {
        #[serde(with = "blaze_protocol::bytes")]
        ipc: Vec<u8>,
        rows: u64,
    },
    /// [`Request::Hot`]'s answer.
    Hot(Hot),
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

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UnitOf {
    /// The daemon's index of the unit in the request's unit graph: dense from 0 and the same
    /// in every response of the request, so a client finds a unit's row by it.
    pub id: u32,
    /// The unit is compiled with `--test` (cargo's `CompileMode::Test`): cargo names it
    /// `` `foo` (lib test) `` in "generated N warnings" and "could not compile".
    pub test: bool,
    pub target: UnitTarget,
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
    /// [`LinkKind::Placed`] when any of [`Worked::links`] is not [`LinkHow::Spliced`].
    pub link: LinkKind,
    /// Functions found in the store.
    pub found: u64,
    /// Queries verified green.
    pub green: u64,
    /// Each image the build linked, in the order it linked them.
    pub links: Vec<Linked>,
    /// Queries executed.
    pub reran: u64,
    /// Each crate of the build that is not a workspace member, sorted by cargo package name:
    /// whether its check outcome and its bodies were served.
    pub served: Vec<ServedCrate>,
    /// Functions whose code was generated.
    pub compiled: u64,
    /// The measured run time of the request's nodes, summed by kind: each kind that ran at
    /// all, in [`NodeKind::ALL`]'s order.
    pub steps_us: Vec<(NodeKind, u64)>,
    /// The plane's critical path of the request.
    pub critical_path_us: u64,
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
    /// Laid out whole although an image was kept: the names of the output's fields that differ
    /// from the kept image's (`exports`, `debug`), or `not incremental` for an output that keeps
    /// no room.
    Full { why: Vec<String> },
}

/// The `link:` line's words: `first link`, `spliced`, `not spliced: <why>, <why>`.
impl std::fmt::Display for LinkHow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkHow::First => f.write_str("first link"),
            LinkHow::Spliced => f.write_str("spliced"),
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
    /// Which blaze the daemon is: a client of another build never sends it a request of one.
    pub build: Build,
    pub views: u32,
    /// The profile history's last error (`blaze_profile::history`): a flush that failed,
    /// naming the file; `None` while every flush has been written.
    pub history: Option<String>,
    pub requests: Vec<u64>,
    pub budget_bytes: u64,
    pub memory_bytes: u64,
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

/// What failed, and where.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct ApiError {
    /// A file, a package, a query, a function: where it failed, as precisely as known.
    pub at: Option<String>,
    pub what: String,
}

impl ApiError {
    pub fn new(what: impl Into<String>, at: Option<String>) -> ApiError {
        ApiError { what: what.into(), at }
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

/// What a change from one view to another reaches, without building it.
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
    Request { from: RequestRef, request: Request },
    /// The connection ended: its requests are cancelled and its replies dropped.
    Gone { conn: Conn },
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
            (blaze_protocol::cbor::to_vec(&Applicability::MachineApplicable).unwrap(), "MachineApplicable"),
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
        let item = blaze_protocol::cbor::to_vec(&Envelope::new(3, Response::Fetched {
            verb: FetchVerb::Updating,
            what: String::new(),
        }))
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
