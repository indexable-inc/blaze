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
//! - [`diagnostics`]: [`Request::Diagnostics`]'s ids, diff and bounded bases.
//! - [`json`]: the rendering for people.
//! - [`activation`]: the socket a service manager passes the daemon, the one unsafe module.
//! - [`units`]: the launchd agent and systemd units that socket-activate it.

#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// The field order promised above, held by `blaze::cbor_field_order`.
#![blaze::canonical_cbor]

pub mod activation;
pub mod borrowed;
pub mod build;
pub mod client;
pub mod diagnostics;
pub mod frames;
pub mod goal;
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
    BasisRef, BuildScriptRanRef, CachedRef, DiagnosticRef, FactReadRef, LiveFrameRef, RanWhyRef,
    ResponseRef, SpanRef, TestResultRef, UnitOfRef, UnitStartedRef, UnitTargetRef,
};
pub use diagnostics::{Base, Found, Seen};
pub use goal::{Goal, Tiers};
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
/// with the in-browser compiler they fed. 49: [`Worked::why`], the build's own summary in its
/// `Work` when the daemon has it by then, so its client makes one round trip. 50:
/// [`Targets::goal`] and [`Worked::tiers`], the build goal ([`Goal`]) and how much of the code
/// a build linked is optimized. 51: [`Worked::storage`] ([`Storage`]: what the daemon's store
/// and its home hold against their budget, on every build's end), `unkept` of
/// [`Request::Gc`], and [`Status::placed_by`]. 52: [`UnitOf::primary`], by which `check --fix` edits only the packages the
/// request asked for. 53: [`Request::Store`] (not the `Request::Pack` that 48 removed: packs of store records, docs/remote-cache.md).
/// 54: [`Cargo::prebuilt`] (`--no-prebuilt`), by which a request takes no record from a pack source.
/// 55: `pid` and `timed` of [`Request::ClientRows`] ([`Timed`]): the thin `blaze`'s spans, by name,
/// sent on the request's own connection; a `ClientRows` of 54 (neither field) reads as pid 0 and
/// no timed spans.
/// 56: [`Request::Diagnostics`] and [`Response::Diagnostics`] ([`Seen`]): a check's errors and
/// warnings against what the client was last told, as new, fixed and unchanged, by stable id
/// (`blaze diagnostics --since`, `blaze agents hook`).
/// 57: [`Request::Prove`] and [`Response::Proven`] ([`Proven`], [`ProofRecord`], [`Subset`]):
/// `blaze prove`, the theorem functions of the crates selected translated to Lean and checked.
/// 58: [`Subset::outcomes`] ([`SubsetFunction`], [`SubsetOutcome`]), each function of the
/// sans-IO cores with its outcome by path, and [`Subset::generic`], the generic functions
/// counted apart from the refused.
/// 59: [`TestResult::not_kept`] ([`NotKept`]), and `TestOutcome::Unreached` is gone: a cached
/// test is [`TestOutcome::Passed`] with [`TestResult::cached`] and `millis` 0; and
/// [`Response::TestsOf`] ([`TestsOf`], [`Near`]): each test executable's count of tests, how
/// many the filter left out, and the nearest names to a filter that matched none.
/// 60: [`TestResult::ran`] ([`RanWhy`]): why a test that ran was not reported cached, by
/// name, and for a changed ask which one with its answers then and now.
/// 61: [`ProofRecord::ModelBroken`]: a theorem whose generated Lean model Lean refuses,
/// answered as blaze's bug with Lean's first error, proof file or none, never `Open`.
pub const SCHEMA: u32 = 61;

/// The most bytes a request the daemon reads may be: CBOR from a peer is decoded only up to
/// this.
pub const LONGEST_REQUEST: u32 = blaze_protocol::LONGEST_FRAME;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[expect(
    blaze::cbor_field_order,
    reason = "`schema` first in the JSON line (`json::request`, pinned by json/tests.rs), so a reader sees the schema before the body; the CBOR is written in key order by hand (`frames::item_into`)"
)]
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
    /// What the request waits for (`--goal edit|max`); `None` for the profile's own
    /// ([`Goal::of_profile`]: `dev` edits, `release` is `max`). A check compiles no program,
    /// so nothing of it reads this.
    pub goal: Option<Goal>,
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
    /// Whether a record the home's store and the machine's layer miss may be fetched from a
    /// pack source (packs.json's `remote`, an R2 bucket): `false` for blaze's `--no-prebuilt`,
    /// which builds every such miss from source for this request, as packs.json's
    /// `"prebuilt": false` does for every request.
    pub prebuilt: bool,
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
            .field("prebuilt", &self.prebuilt)
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
    /// `blaze gc` while the daemon holds the collector's areas: one run of the daemon's
    /// collector to its end (`dry_run` plans and removes nothing), and one lap of its store's
    /// collector (docs/gc.md, "The store's budget and collector"), answered with
    /// [`Response::Collected`] as they end. `unkept` (`blaze clean --unkept`): the store's lap
    /// forgets every record no pin and no root keeps, whatever the budget and its pack's age.
    /// Answered by the daemon's loop, which holds the collectors.
    Gc {
        unkept: bool,
        dry_run: bool,
    },
    /// `blaze store export|import|install ...` (docs/remote-cache.md, "Packs") while a daemon
    /// holds the home's store: `words` are the command's words after `store`, run by the
    /// daemon over its own store as the CLI runs them over a store no daemon holds. Answered
    /// with what it did as note [`Response::Diagnostic`]s, or its error, by the daemon's
    /// loop, which holds the store.
    Store {
        words: Vec<String>,
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
    /// `blaze diagnostics --since TOKEN` and the agents' after-write hook (`blaze agents
    /// hook`): `view` checked as [`Request::Check`] checks it, and its errors and warnings
    /// answered with one [`Response::Diagnostics`] against the ones this `client` (an agent's
    /// session, a shell) was last told of the same checkout and scope (the targets' packages,
    /// features, profile, target and command line: [`diagnostics::scope`]): new, fixed and
    /// unchanged, each under an id that an edit moving lines leaves alone ([`diagnostics::id`]).
    /// No `since` answers everything as new; a `since` the daemon no longer holds answers
    /// everything as new and says it did not know it ([`diagnostics::Base::Unknown`]). The
    /// daemon holds the last answer per (checkout, scope, client), at most
    /// [`diagnostics::BASES`] of them and [`diagnostics::HELD`] diagnostics together, the
    /// least recently answered dropped first. At `wait_ms` (0: no bound) with the check still
    /// running, it answers `ended: false`, keeps the base, and lets the check go on. A check that
    /// could not run answers with its error as `failed`, the base kept. No other response is
    /// sent: not the check's own diagnostics, units or work.
    Diagnostics {
        view: ViewRef,
        since: Option<ContentId>,
        client: String,
        targets: Targets,
        wait_ms: u64,
    },
    /// What a client recorded of build `build` (its [`Worked::build_id`]), sent as the request
    /// that asked for it ends: its own `spans`, `events` and `folded` rows, each an Arrow IPC
    /// stream of the profile schema's table (empty for none), every row's `build_id` the
    /// build's, and when it started and had its answer, on the profile's clock
    /// (`blaze_profile::clock::mono_ns`). The daemon's loop hands them to its profile history,
    /// which writes them as the client's process and gives the build's row the two times
    /// (`builds.client_started_ns`, `client_ended_ns`); answered with `Done { ok: true }` once
    /// written. A daemon that keeps no history answers at once and keeps nothing.
    ///
    /// `pid` is the client's process and `timed` its spans by name ([`Timed`]): what a client
    /// with no flight recorder of its own sends (the thin `blaze`, which links no Arrow), made
    /// the client's `spans` rows by the daemon. A client sends its spans one way: `spans` or
    /// `timed`, never both (the daemon refuses both, naming the build).
    ///
    /// The fields are declared in their keys' order (by length, then bytes), as deterministic
    /// CBOR writes them.
    ClientRows {
        /// 0 in a `ClientRows` of schema 54, which had no such field.
        #[serde(default)]
        pid: u32,
        build: u64,
        #[serde(with = "blaze_protocol::bytes")]
        spans: Vec<u8>,
        /// Empty in a `ClientRows` of schema 54, which had no such field.
        #[serde(default)]
        timed: Vec<Timed>,
        #[serde(with = "blaze_protocol::bytes")]
        events: Vec<u8>,
        #[serde(with = "blaze_protocol::bytes")]
        folded: Vec<u8>,
        ended_ns: i64,
        started_ns: i64,
    },
    /// `blaze prove`: every function marked `#[blaze::theorem]` in the library crates `targets`
    /// selects (as [`Request::Check`] selects them), translated to Lean from the view's front
    /// end (`hir_ty_lean::emit`), and unless `emit`, its proof checked by the Lean at `lean`
    /// (an absolute path; the client reads it from `BLAZE_LEAN_PROVER`) against the crate's
    /// proof file under `lean/` beside its `src/`. Nothing is compiled: the view's proc macros
    /// and build scripts' runs, which its front end applies before it is read, are built
    /// first. With `subset`, also the count of what the translator takes over every function
    /// in a `#![blaze::sans_io]` module of those crates. Answered with [`Response::Proven`].
    Prove {
        emit: bool,
        lean: Option<String>,
        view: ViewRef,
        subset: bool,
        targets: Targets,
    },
}

/// One span of a client's, by its name, on the profile's clock (`blaze_profile::clock::mono_ns`):
/// from `start_ns` to `end_ns` ([`Request::ClientRows::timed`]). The names are the client's
/// fixed set (`client: connect`, ...), each said once a command.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Timed {
    // In their keys' order, as deterministic CBOR writes them.
    pub name: String,
    pub end_ns: i64,
    pub start_ns: i64,
}

impl Timed {
    /// The name of the client's span that ends as it becomes the program (`exec`): its end is
    /// the instant the program's `program: exec to Hello` starts at.
    pub const ANSWER_TO_EXEC: &'static str = "client: answer to exec";

    /// Where a `run`'s client became its program: the end of its [`Timed::ANSWER_TO_EXEC`],
    /// `None` for a client that ran none (a `build`).
    pub fn exec_of(timed: &[Timed]) -> Option<i64> {
        timed.iter().find(|it| it.name == Timed::ANSWER_TO_EXEC).map(|it| it.end_ns)
    }
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
    /// (it failed or was stopped), and nothing is kept under it. Whether its own compute failed
    /// is not recorded here: that build's `errors` rows say.
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
    /// [`Request::Diagnostics`]'s one answer.
    Diagnostics(Seen),
    /// [`Request::Prove`]'s one answer.
    Proven(Proven),
    /// What a test run asked of one test executable, said once per executable: what a client
    /// refusing a run of none names.
    TestsOf(TestsOf),
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

    /// A warning of the whole request, of no unit, span or code: `warning: {message}`.
    pub fn warning(message: String) -> Diagnostic {
        Diagnostic {
            level: Level::Warning,
            rendered: format!("warning: {message}\n"), // alloc: the report owns it, once per warning
            ..Diagnostic::note(message)
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
    /// The unit's package is one the request asked for (cargo's `is_primary_package`), not
    /// one built or checked only because such a package depends on it. `check --fix` edits
    /// the files of these alone: a dependency in the checkout (another member, a vendored
    /// crate) is checked, and its diagnostics shown, but it is not what was asked to be fixed.
    pub primary: bool,
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
    /// Why it computed again what it did, as the daemon sums it up ([`Rebuilt::summary`], a
    /// report a root cause), when the daemon had explained the build by the time this was
    /// said: the client then asks for no summary. `None` when it had not (the answer is never
    /// held for the explanation): the client asks, as before ([`Request::Rebuilt`] with
    /// `build`), unless [`Worked::misses`] is 0.
    pub why: Option<Vec<Report>>,
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
    /// The build's goal and how much of the code of the images it linked is optimized
    /// ([`Tiers::line`]: `goal edit: tier 0 · 38% optimized`). `None` when no unit the request
    /// asked for compiles a program.
    pub tiers: Option<Tiers>,
    /// The steps it computed again instead of reusing, each once: the misses `blaze why`
    /// explains and [`Rebuilt::summary`] sums up. 0 says that summary is empty, so the
    /// build's client asks for none ([`Request::Rebuilt`] with `build`).
    pub misses: u64,
    /// How the files of the outputs it placed in the build dir got there.
    pub placed: PlacedBytes,
    /// Each crate of the build that is not a workspace member, sorted by cargo package name:
    /// whether its check outcome and its bodies were served.
    pub served: Vec<ServedCrate>,
    /// What the daemon's store and its home hold against their budget as the build ends.
    /// `None` from a daemon that collects nothing (a test's, or one that is not `blaze
    /// serve`'s).
    pub storage: Option<Storage>,
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

/// What blaze holds on disk as a build ends, against its budget (docs/gc.md, "The store's
/// budget and collector"): data on every build, for whoever renders it. Bytes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Storage {
    /// One line to print, when the daemon's policy says this build should say something
    /// (held at 85% of the budget or more, or the volume low; at most once in six hours, once
    /// an hour when over): it names the command that raises the budget and the one that
    /// frees space. `None`: nothing to say.
    pub warning: Option<String>,
    /// Everything the home holds (its build dirs, its store, its image), and its budget: the
    /// tightest of an absolute size, a share of the volume and what leaves the volume's floor
    /// free. The share used is `held_bytes / budget_bytes`. A budget of 0: not measured yet.
    pub held_bytes: u64,
    /// The collector looked at everything and what is held is still at the budget or past
    /// it: what is left is in use or kept.
    pub over_budget: bool,
    /// The daemon's store, and how much it grew or shrank since the build before ended.
    pub store_bytes: u64,
    pub budget_bytes: u64,
    /// What the store's collector could give back: dead bytes, and records no root keeps.
    pub collectable_bytes: u64,
    pub store_delta_bytes: i64,
    /// The volume's free space.
    pub volume_free_bytes: u64,
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

/// One test's result. Its fields are declared in the order CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TestResult {
    /// It ran although selection was asked: why no kept pass of it held. `None` for a cached
    /// result, an ignored test and a run selection did not judge.
    pub ran: Option<RanWhy>,
    pub name: String,
    /// The test executable, as cargo's `Running` line names it.
    pub binary: String,
    /// Not run: the pass that stands, and what it was kept by. A cached result is
    /// [`TestOutcome::Passed`] with `millis` 0; only a pass is ever kept.
    pub cached: Option<Cached>,
    pub millis: u64,
    #[serde(with = "blaze_protocol::bytes")]
    pub output: Vec<u8>,
    pub outcome: TestOutcome,
    /// It ran outside the sandbox (docs/cargo.md), so the command's summary names it. False for
    /// a cached result, whose run is not this one.
    pub outside: bool,
    /// A pass that ran and was not kept, so the test runs again next time: why. `None` for a
    /// failure (never kept), a cached result and a pass that was kept.
    pub not_kept: Option<NotKept>,
    /// The daemon's index of `binary` among the request's test executables: dense from 0, the
    /// same for every result of one executable, so a client groups results by it.
    pub executable: u32,
}

/// What a test run asked of one test executable ([`Response::TestsOf`]), one per executable
/// of the request whose tests were selected: how many tests it has, how many the name
/// filter left out, and, when the filter left out every one, its tests nearest the filter. A
/// client that refuses a run of none says the counts and the nearest names from these. Its
/// fields are declared in the order CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TestsOf {
    /// Its tests, ignored ones included.
    pub tests: u32,
    /// The test executable, as [`TestResult::binary`] names it.
    pub binary: String,
    /// When the filter left out every one of its tests: those nearest it, nearest first, at
    /// most three. Empty otherwise.
    pub nearest: Vec<Near>,
    /// As [`TestResult::executable`].
    pub executable: u32,
    /// Those the name filter left out.
    pub filtered_out: u32,
}

/// A test's name near a filter that matched none ([`TestsOf::nearest`]): `distance` is the
/// edit distance (rustc's, with substrings) between a filter and its whole path or its last
/// segment, so a client ranks the nearest of every executable together.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Near {
    pub name: String,
    pub distance: u32,
}

/// Why a test ran rather than being reported cached (blaze-testsel's `machine::Missed`):
/// `why` is the stable word a tool matches on (`run-all`, `no-head`, `unknown-state`,
/// `no-row`, `function-changed`, `read-changed`) and `said` the sentence a person reads. For a
/// changed ask, `ask` names it (`fn` and its id, or `file`, `dir` or `env` and its name) and
/// `before` and `now` are its answers at the kept pass and now, as short ids (`absent` for
/// nothing there, `gone` for a function no longer in the binary); `None` where not known. Its
/// fields are declared in the order CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RanWhy {
    pub ask: Option<String>,
    pub now: Option<String>,
    pub why: String,
    pub said: String,
    pub before: Option<String>,
}

/// Why a pass was not kept (blaze-testsel's `machine::NotKept`): `no` is the stable word a
/// tool matches on (`clock`, `unsandboxed`, `doctest`, ...), `why` the sentence a person reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NotKept {
    pub no: String,
    pub why: String,
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
    /// How the daemon's store places outputs, decided once as it opened (by cloning where
    /// its file system shares blocks, else by copying), in one sentence; `None` until it has
    /// decided, and from a daemon that says none.
    pub placed_by: Option<String>,
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

/// What `blaze prove` found ([`Request::Prove`]): one record per theorem function, sorted
/// (by function, then the whole record), so the same tree answers the same bytes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Proven {
    /// Why no proof was checked, where none could be: no Lean was named, the one named is not
    /// the pinned release, a file could not be written, or Lean refused blaze's own library.
    /// The records are then the refusals alone, and the command fails with this.
    pub failed: Option<String>,
    /// With [`Request::Prove`]'s `subset`: what the translator takes of the sans-IO cores.
    pub subset: Option<Subset>,
    pub records: Vec<ProofRecord>,
}

/// One theorem function's outcome. `function` is the translator's path of it
/// (`krate::module::name`); a refusal in a function it calls names that function.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ProofRecord {
    /// Not proved: no proof file at `proof_file`, or Lean refused it (or the model, or the
    /// check that it proves `statement`); `why` is that, with Lean's whole output, whose
    /// remaining goals are what the proof's writer reads.
    Open { why: String, function: String, statement: String, proof_file: String },
    /// The pinned Lean's kernel accepted the proof of `theorem`, resting only on `axioms`
    /// (among Lean's three). `key` is the proof's identity (`hir_ty_lean::proof_key`): every
    /// input the verdict depends on. `model` is the generated model's file.
    Proved {
        key: ContentId,
        model: String,
        axioms: Vec<String>,
        theorem: String,
        function: String,
    },
    /// `--emit`: the model and statement written, nothing checked. `path` is the model's file,
    /// `module` its Lean module.
    Emitted { path: String, module: String, function: String, statement: String },
    /// The translator refused it: what construct, and where (`hir_ty_lean::NotTranslated`).
    Refused { at: String, what: String, function: String },
    /// Lean refused the generated model of the theorem (`module`: its own, or one it
    /// imports): a bug in blaze's translator, which no proof file can fix. Given whether or
    /// not there is a proof file. `at` is where Lean's first error is (`<file>:<line>:<column>`,
    /// the file absolute) and `error` the rest of it as Lean printed it (`error(<code>): ...`).
    ModelBroken { at: String, error: String, module: String, function: String },
}

/// What the translator takes of every function in a `#![blaze::sans_io]` module of the crates
/// proved, theorem or not: what decides the subset's next step
/// (docs/v2/plan/design/proofs-for-sans-io.md, section 3).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Subset {
    /// Generic functions, which the translator takes at each instance a theorem reaches and
    /// alone at none: neither translated here nor refused.
    pub generic: u32,
    /// Each refusal's `what` with the number of functions refused for it, most first (then by
    /// `what`). A function is counted under the first refusal the translator meets; how many
    /// it would meet past that one is not known. A generic function is not among them: it is
    /// counted in `generic`.
    pub refused: Vec<SubsetRefusal>,
    /// Every function counted, with its outcome, sorted by path (two of one path, two impls'
    /// methods of one name, in the item table's key order).
    pub outcomes: Vec<SubsetFunction>,
    /// Every function counted: `translated + generic` and the refused counts add up to it.
    pub functions: u32,
    pub translated: u32,
}

/// One function of a sans-IO core and what the translator made of it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SubsetFunction {
    /// Its path (`blaze_core::world::disk::Disk::find`).
    pub path: String,
    pub outcome: SubsetOutcome,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum SubsetOutcome {
    Translated,
    /// A generic function: translated at each instance a theorem reaches, none asked for here.
    Generic,
    /// Refused for `what`, the first construct the translator does not take.
    Refused { what: String },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SubsetRefusal {
    pub what: String,
    pub count: u32,
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

    /// The invariant: a `ClientRows` written by schema 54 (no `pid`, no `timed`) reads as the
    /// same rows with pid 0 and no timed spans, and one of 55 round-trips whole. Control: the
    /// 55 value with its `timed` dropped is not equal to it, so the round trip would see a lost
    /// field.
    #[test]
    fn client_rows_of_the_layout_before_timed_spans_still_read() {
        #[derive(Serialize)]
        enum Before {
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
        let before = Before::ClientRows {
            build: 9,
            spans: vec![1],
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns: 20,
            started_ns: 10,
        };
        let bytes = blaze_protocol::cbor::to_vec(&before).unwrap();
        let read: Request = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        let old = Request::ClientRows {
            build: 9,
            spans: vec![1],
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns: 20,
            started_ns: 10,
            pid: 0,
            timed: Vec::new(),
        };
        assert_eq!(read, old);

        let timed = vec![Timed { name: Timed::ANSWER_TO_EXEC.to_owned(), start_ns: 11, end_ns: 19 }];
        let new = Request::ClientRows {
            build: 9,
            spans: Vec::new(),
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns: 20,
            started_ns: 10,
            pid: 41,
            timed: timed.clone(), // clone: the spans, kept to compare, once a test
        };
        let bytes = blaze_protocol::cbor::to_vec(&new).unwrap();
        let back: Request = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(back, new);
        assert_eq!(Timed::exec_of(&timed), Some(19));
        // clone: the request, to plant a lost span in, once a test
        let Request::ClientRows { timed: mut planted, .. } = new.clone() else { unreachable!() };
        planted.clear();
        assert_ne!(Some(planted), Some(timed), "the control: a dropped span is seen");
    }

    /// `Request::Diagnostics` and its answer are written in the codec's key order (shorter
    /// keys first, then bytewise): the answer's bytes pinned whole, the request's keys in order.
    #[test]
    fn diagnostics_since_is_written_in_key_order() {
        let seen = Response::Diagnostics(Seen {
            new: Vec::new(),
            base: Base::First,
            root: ContentId([0; 32]),
            build: None,
            ended: true,
            fixed: Vec::new(),
            token: None,
            failed: None,
            unchanged: Vec::new(),
        });
        let bytes = blaze_protocol::cbor::to_vec(&seen).unwrap();
        let mut want = vec![0xa1, 0x6b];
        want.extend_from_slice(b"Diagnostics");
        want.extend_from_slice(&[0xa9, 0x63, b'n', b'e', b'w', 0x80]);
        want.extend_from_slice(&[0x64, b'b', b'a', b's', b'e', 0x65, b'F', b'i', b'r', b's', b't']);
        want.extend_from_slice(&[0x64, b'r', b'o', b'o', b't', 0x58, 0x20]);
        want.extend_from_slice(&[0; 32]);
        want.extend_from_slice(&[0x65, b'b', b'u', b'i', b'l', b'd', 0xf6]);
        want.extend_from_slice(&[0x65, b'e', b'n', b'd', b'e', b'd', 0xf5]);
        want.extend_from_slice(&[0x65, b'f', b'i', b'x', b'e', b'd', 0x80]);
        want.extend_from_slice(&[0x65, b't', b'o', b'k', b'e', b'n', 0xf6]);
        want.extend_from_slice(&[0x66, b'f', b'a', b'i', b'l', b'e', b'd', 0xf6]);
        want.push(0x69);
        want.extend_from_slice(b"unchanged");
        want.push(0x80);
        assert_eq!(bytes, want, "{bytes:02x?}");
        assert_eq!(blaze_protocol::cbor::from_slice::<Response>(&bytes).unwrap(), seen);

        let request = Request::Diagnostics {
            view: ViewRef::Head("/w".to_owned()),
            since: Some(ContentId([3; 32])),
            client: "s".to_owned(),
            targets: Targets {
                goal: None,
                cargo: None,
                target: None,
                profile: "dev".to_owned(),
                features: Vec::new(),
                packages: vec!["demo".to_owned()],
                all_features: false,
                no_default_features: false,
            },
            wait_ms: 20_000,
        };
        let bytes = blaze_protocol::cbor::to_vec(&request).unwrap();
        let at = |key: &str| {
            let mut head = vec![0x60 | key.len() as u8];
            head.extend_from_slice(key.as_bytes());
            bytes.windows(head.len()).position(|it| it == head).unwrap_or_else(|| panic!("{key}"))
        };
        let keys = ["view", "since", "client", "targets", "wait_ms"].map(at);
        assert!(keys.is_sorted(), "{keys:?}: {bytes:02x?}");
        assert!(bytes.ends_with(&[0x67, b'w', b'a', b'i', b't', b'_', b'm', b's', 0x19, 0x4e, 0x20]));
        assert_eq!(blaze_protocol::cbor::from_slice::<Request>(&bytes).unwrap(), request);
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
            ran: None,
            name: "t".to_owned(),
            binary: "b".to_owned(),
            cached: None,
            not_kept: None,
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
            prebuilt: false,
        };
        let bytes = blaze_protocol::cbor::to_vec(&cargo).unwrap();
        let files = [0x65, b'f', b'i', b'l', b'e', b's', 0x81, 0x82, 0x62, b'/', b'c', 0x42, 1, 2];
        assert!(bytes.windows(files.len()).any(|it| it == files), "{bytes:02x?}");
        let width = [0x65, b'w', b'i', b'd', b't', b'h', 0x18, 100];
        assert!(bytes.windows(width.len()).any(|it| it == width), "{bytes:02x?}");
        let prebuilt = [0x68, b'p', b'r', b'e', b'b', b'u', b'i', b'l', b't', 0xf4];
        assert!(bytes.ends_with(&prebuilt), "the longest key last: {bytes:02x?}");
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
