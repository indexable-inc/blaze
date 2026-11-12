//! The hub's one schema (rule no-mcp): every message is an [`Envelope`] of [`SCHEMA`], written
//! in RFC 8949's core deterministic CBOR ([`encode`], [`decode`]) and framed as the daemon's
//! own messages are (`blaze_protocol::write_frame`), over a Unix socket or the body of an HTTP
//! request. The command line (`blaze hub ..`) is a client of the same messages.
//!
//! Every struct and struct variant declares its fields in the order of their keys' encoding
//! (shorter names first, then bytewise), as blaze-api's do.
//!
//! Everything here is read from a peer, so everything has a limit, and [`crate::hub::Hub`]
//! holds each message to them before it changes anything: [`FRAME_MOST`] is the driver's (a
//! frame past it is never read), the rest are the core's.

use alloc::{string::String, vec::Vec};
use core::fmt;

use blaze_vfs::{Mode, Op};
use bstd::id::ContentId;

use crate::tree::TreeError;
use serde::{Deserialize, Serialize};

/// The version of every body's schema: a reader of another refuses the message by name. A
/// change to any type here is a new number.
pub const SCHEMA: u32 = 1;

/// The most bytes one frame carries, the daemon's own bound (`blaze_protocol::LONGEST_FRAME`):
/// CBOR from a peer is decoded only up to this.
pub const FRAME_MOST: usize = 16 << 20;
/// The most bytes one file of a lane may be: a write is one frame's, so a file past this is
/// refused by name until a file can be written in parts (hub.md, "Files").
pub const BLOB_MOST: u64 = 8 << 20;
/// The most ops one [`Change`] carries.
pub const OPS_MOST: usize = 4096;
/// The most bytes a [`Request::Read`] answers with; a longer file is read in parts.
pub const READ_MOST: u64 = 1 << 20;
/// The most entries a [`Request::List`] answers with.
pub const LIST_MOST: usize = 1024;
/// The most bytes a lane may write over its life: what bounds the store a lane can fill.
pub const WROTE_MOST: u64 = 4 << 30;
/// How many of a lane's last roots a job may still be asked of.
pub const ROOTS_KEPT: usize = 16;
/// How many of a lane's ended jobs `inspect` still shows.
pub const JOBS_KEPT: usize = 16;
/// The most bytes a lane's name, or the name of who owns it, may be.
pub const NAME_MOST: usize = 64;
/// The most bytes a job's task may be.
pub const TASK_MOST: usize = 256;
/// The most bytes a job's targets may be.
pub const TARGETS_MOST: usize = 1 << 20;

/// One frame's item: a body of [`SCHEMA`], asking or answering request `id`. An answer and
/// every event of a job carry the id of the request they belong to.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub id: u64,
    pub body: T,
    pub schema: u32,
}

impl<T> Envelope<T> {
    pub fn new(id: u64, body: T) -> Envelope<T> {
        Envelope { id, body, schema: SCHEMA }
    }
}

/// Why bytes are no message of this schema.
#[derive(PartialEq, Eq, Debug)]
pub enum WireError {
    /// The frame is longer than [`FRAME_MOST`]; nothing of it was decoded.
    TooLong { len: usize },
    /// The bytes are not one item of deterministic CBOR, or not this type.
    Cbor(blaze_protocol::cbor::Error),
    /// A message of another schema.
    Schema { got: u32 },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooLong { len } => write!(
                f,
                "a hub frame of {len} bytes is past the {FRAME_MOST} a frame may be (wire::FRAME_MOST)"
            ),
            WireError::Cbor(error) => write!(f, "a hub frame does not decode: {error}"),
            WireError::Schema { got } => write!(
                f,
                "a hub message of schema {got}, where this hub speaks schema {SCHEMA}: update the \
                 older of the two"
            ),
        }
    }
}

impl core::error::Error for WireError {}

/// `message` as one frame's item.
pub fn encode<T: Serialize>(message: &Envelope<T>) -> Result<Vec<u8>, WireError> {
    blaze_protocol::cbor::to_vec(message).map_err(WireError::Cbor)
}

/// One frame's item read back: refused past [`FRAME_MOST`] before anything is decoded, where
/// it is not deterministic CBOR, and where its schema is not [`SCHEMA`].
pub fn decode<'de, T: Deserialize<'de>>(frame: &'de [u8]) -> Result<Envelope<T>, WireError> {
    if frame.len() > FRAME_MOST {
        return Err(WireError::TooLong { len: frame.len() });
    }
    let message: Envelope<T> =
        blaze_protocol::cbor::from_slice(frame).map_err(WireError::Cbor)?;
    if message.schema != SCHEMA {
        return Err(WireError::Schema { got: message.schema });
    }
    Ok(message)
}

/// What an agent asks of the hub. Who asks is not in the message: the driver names the
/// authenticated sender to the core beside it. An agent needs no checkout: it reads and
/// changes its lane through these, and every call that changes a lane answers with the lane's
/// new root, the name of the exact tree every later answer is about.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum Request {
    /// Every lane, a row each: `blaze hub ls`.
    Ls,
    /// A check, build or test of a lane at a root; answered with [`Response::Queued`], then
    /// [`Response::Started`] and [`Response::Ended`] under the same id.
    Job(JobSpec),
    /// A new lane `lane` at the root `from` is at now, on `from`'s base: a second approach
    /// tried beside the first. It copies a root and nothing else.
    Fork { from: String, lane: String },
    /// Entries of the directory `path` (`""` for the root) after the name `after`, in name
    /// order, at most [`LIST_MOST`]; answered with [`Response::Listed`].
    List { lane: String, path: String, after: Option<String> },
    /// A new lane on `base`, a root the hub holds (a commit's tree it imported); answered
    /// with [`Response::Opened`]. Opening a lane its asker owns on the base it has changes
    /// nothing.
    Open { base: ContentId, lane: String },
    /// `len` bytes of the file `path` from `at`, at most [`READ_MOST`]; answered with
    /// [`Response::File`].
    Read { at: u64, len: u64, lane: String, path: String },
    /// Ends a lane: its root and its jobs are let go.
    Close { lane: String },
    /// One transaction: every op applied in order, or none; answered with
    /// [`Response::Root`].
    Change(Change),
    /// Stops a job.
    Cancel { job: u64 },
    /// The hub as a whole: `blaze hub status`.
    Status,
    /// One lane, to its owner: `blaze hub inspect <lane>`.
    Inspect { lane: String },
}

/// A transaction on a lane: writes, range edits (a patch), deletes and renames of any number
/// of files, which make one new root together or leave the lane as it was.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Change {
    /// The root the ops were written against. `Some`: the change is refused, naming both,
    /// when the lane is no longer there. `None`: applied to the lane as it is.
    pub on: Option<ContentId>,
    /// The VFS's own ops, at most [`OPS_MOST`]: `Write` (a whole file), `Edit` (`len` bytes
    /// at `at` replaced), `Delete` (a file, or a directory with everything under it),
    /// `Rename`.
    pub ops: Vec<Op>,
    pub lane: String,
}

/// Which of the daemon's builds a job is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Check,
    Build,
    Test,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Check => "check",
            Kind::Build => "build",
            Kind::Test => "test",
        }
    }
}

/// A job (rule every-agent-has-a-task): `task` is the work it is for, its check is `kind` of
/// `targets` on the lane as it is at `at`, and it ends in exactly one [`Outcome`].
///
/// The hub, not the client, names the view the job builds: the lane's own. A request cannot
/// name another lane's files.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// The root the verdict is asked of: the lane's, or one of its last [`ROOTS_KEPT`]. Any
    /// other is a refusal naming both: no job judges a tree other than the one named.
    pub at: ContentId,
    /// The schema of blaze-api `targets` is written in.
    pub api: u32,
    /// For [`Kind::Test`]: every test, not only the tests the lane's change reaches. With
    /// another kind it is refused, not ignored.
    pub all: bool,
    pub kind: Kind,
    pub lane: String,
    /// The objective or the named piece of one this job is for.
    pub task: String,
    /// What to build: one `blaze_api::Targets`, as CBOR of schema `api`. The hub hands it to
    /// the daemon as it came.
    #[serde(with = "blaze_protocol::bytes")]
    pub targets: Vec<u8>,
}

/// What the hub answers.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum Response {
    /// Part of a file, as the lane has it at `root`.
    File(FilePart),
    /// The lane is at `root` after its change `seq`. `check` is the job the hub started on
    /// the new root by itself, where it checks eagerly: its verdict is awaited by that id,
    /// or by asking for a check at `root`.
    Root { seq: u64, lane: String, root: ContentId, check: Option<u64> },
    /// The job ended: its last message.
    Ended { job: u64, outcome: Outcome },
    Lane(Detail),
    Lanes(Vec<Row>),
    Closed { lane: String },
    Listed(Listed),
    /// The lane is open at `root` after its change `seq` (0 for none).
    Opened { seq: u64, lane: String, root: ContentId },
    /// The job is in the ledger as `job`: a new one, or the live one that already asks the
    /// same of the same root.
    Queued { job: u64, lane: String },
    Status(Status),
    /// A machine took the job.
    Started { job: u64, warmth: Warmth, machine: String },
    Refused(Refusal),
    Cancelled { job: u64 },
}

/// Bytes `at..at + bytes.len()` of a file of `size` bytes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FilePart {
    pub at: u64,
    /// The whole file's content id (`blaze_vfs::FILE_DOMAIN`).
    pub file: ContentId,
    pub mode: Mode,
    pub path: String,
    pub root: ContentId,
    pub size: u64,
    #[serde(with = "blaze_protocol::bytes")]
    pub bytes: Vec<u8>,
}

/// A file's bytes may be a secret: Debug says how many there are.
impl fmt::Debug for FilePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FilePart({:?} at {}, {} of {} bytes)", self.path, self.at, self.bytes.len(), self.size)
    }
}

/// A directory's entries after `after`, in name order.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Listed {
    /// Whether entries follow the last one here: ask again with `after` its name.
    pub more: bool,
    pub path: String,
    pub root: ContentId,
    pub entries: Vec<Listing>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    /// A file's content id, a directory's root.
    pub id: ContentId,
    pub kind: crate::tree::Kind,
    pub name: String,
}

/// A request the hub will not do, naming the lane, the thing and the rule.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub why: Why,
    pub lane: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum Why {
    /// A lane's or an owner's name is 1 to [`NAME_MOST`] of `a-z`, `0-9`, `-`, `_` and `.`,
    /// starting with a letter or a digit.
    BadName { name: String },
    /// Op `op` of a change (from 0) cannot be done; none of the change was.
    Op { op: u32, error: TreeError },
    /// A read or a list that cannot be answered.
    Tree(TreeError),
    TooManyOps { ops: u64 },
    /// The lane has written [`WROTE_MOST`] bytes.
    WroteTooMuch { bytes: u64 },
    /// The change was written against `on`, and the lane is at `is`.
    Moved { is: ContentId, on: ContentId },
    /// The lane is another's.
    NotYours,
    /// A lane of this name is open on another base.
    Open { base: ContentId },
    /// The hub does not hold this base: it takes its bases from the repository itself.
    UnknownBase { base: ContentId },
    TooManyLanes { most: u64 },
    /// A job with no task, or one past [`TASK_MOST`].
    NoTask,
    /// `all` on a job that is not a test.
    AllNotTest { kind: Kind },
    TargetsTooBig { len: u64 },
    /// The job names a root the lane is not at and was not at lately.
    NotAt { at: ContentId, is: ContentId },
    NoSuchJob { job: u64 },
    NoSuchLane,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lane = &self.lane;
        match &self.why {
            Why::BadName { name } => write!(
                f,
                "the name {name:?} is refused: a lane's or an owner's name is 1 to {NAME_MOST} of \
                 a-z, 0-9, '-', '_' and '.', starting with a letter or a digit"
            ),
            Why::Op { op, error } => write!(
                f,
                "lane {lane}: op {op} of the change cannot be done, so none of it was: {error}"
            ),
            Why::Tree(error) => write!(f, "lane {lane}: {error}"),
            Why::TooManyOps { ops } => write!(
                f,
                "lane {lane}: a change of {ops} ops is past the {OPS_MOST} one change may carry \
                 (wire::OPS_MOST); send it as several"
            ),
            Why::WroteTooMuch { bytes } => write!(
                f,
                "lane {lane}: with this change it would have written {bytes} bytes, past the \
                 {WROTE_MOST} a lane may write over its life (wire::WROTE_MOST); fork it into a \
                 new lane"
            ),
            Why::Moved { is, on } => write!(
                f,
                "lane {lane}: the change was written against root {on:x} and the lane is at \
                 {is:x}; read what changed and send the change against {is:x}"
            ),
            Why::NotYours => write!(f, "lane {lane} is another's: only its owner may use it"),
            Why::Open { base } => write!(
                f,
                "lane {lane} is open already, on the base {base:x}; close it or take another name"
            ),
            Why::UnknownBase { base } => write!(
                f,
                "lane {lane}: the hub does not hold the base {base:x}; a lane's base is the root \
                 of a commit the hub has imported"
            ),
            Why::TooManyLanes { most } => write!(
                f,
                "lane {lane}: the hub holds its {most} lanes already; close one"
            ),
            Why::NoTask => write!(
                f,
                "lane {lane}: a job names the task it is for, in 1 to {TASK_MOST} bytes (rule \
                 every-agent-has-a-task)"
            ),
            Why::AllNotTest { kind } => write!(
                f,
                "lane {lane}: `all` is for a test, and this job is a {}",
                kind.name()
            ),
            Why::TargetsTooBig { len } => write!(
                f,
                "lane {lane}: a job's targets of {len} bytes are past the {TARGETS_MOST} they may \
                 be (wire::TARGETS_MOST)"
            ),
            Why::NotAt { at, is } => write!(
                f,
                "lane {lane}: the job asks for a verdict on root {at:x}, which is not the lane's \
                 root ({is:x}) nor one of its last {ROOTS_KEPT}"
            ),
            Why::NoSuchJob { job } => write!(f, "the hub has no live job {job}"),
            Why::NoSuchLane => write!(f, "the hub has no lane {lane}"),
        }
    }
}

impl core::error::Error for Refusal {}

/// How a job ended.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum Outcome {
    Passed,
    /// The check, build or tests failed, with this many errors or failed tests.
    Failed { errors: u32 },
    /// The daemon would not do it, and says why.
    Refused { why: String },
    Cancelled,
    /// A later job of the same kind on the lane took its place.
    Superseded { by: u64 },
    /// It did not finish, and why: its lane closed, its machine went away. Never read as a
    /// pass.
    Lost { why: String },
}

/// Whether the machine that took a job already held what the job's base needs.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Warmth {
    Warm,
    Cold(Cold),
}

/// Why a job ran cold: the miss's reason, counted per lane.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cold {
    /// No machine had built the lane's base.
    Base,
    /// A warm machine had it and the job went elsewhere: load, or no memory headroom.
    Moved,
    /// The machine had the base's results and dropped them for memory.
    Evicted,
    /// The machine's daemon started since it last built the base.
    Restart,
    /// The base was built, for other targets, features or profile.
    Targets,
    /// The machine's blaze or toolchain is not the one the base was built with.
    Toolchain,
}

/// A lane's jobs that started, by warmth: `jobs` is `warm` plus every field of `cold`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub cold: Colds,
    pub jobs: u64,
    pub warm: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Colds {
    pub base: u64,
    pub moved: u64,
    pub evicted: u64,
    pub restart: u64,
    pub targets: u64,
    pub toolchain: u64,
}

impl Colds {
    pub fn total(&self) -> u64 {
        self.base + self.moved + self.evicted + self.restart + self.targets + self.toolchain
    }
}

impl Counts {
    /// One more job that started with `warmth`.
    pub fn count(&mut self, warmth: Warmth) {
        self.jobs += 1;
        match warmth {
            Warmth::Warm => self.warm += 1,
            Warmth::Cold(Cold::Base) => self.cold.base += 1,
            Warmth::Cold(Cold::Moved) => self.cold.moved += 1,
            Warmth::Cold(Cold::Evicted) => self.cold.evicted += 1,
            Warmth::Cold(Cold::Restart) => self.cold.restart += 1,
            Warmth::Cold(Cold::Targets) => self.cold.targets += 1,
            Warmth::Cold(Cold::Toolchain) => self.cold.toolchain += 1,
        }
    }

    pub fn add(&mut self, other: &Counts) {
        self.jobs += other.jobs;
        self.warm += other.warm;
        self.cold.base += other.cold.base;
        self.cold.moved += other.cold.moved;
        self.cold.evicted += other.cold.evicted;
        self.cold.restart += other.cold.restart;
        self.cold.targets += other.cold.targets;
        self.cold.toolchain += other.cold.toolchain;
    }
}

/// A lane as anyone the hub knows may see it: its name, whose it is and its numbers; never
/// its paths, its bytes or its root.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// How many changes it has taken.
    pub seq: u64,
    pub base: ContentId,
    pub lane: String,
    /// The lane's jobs queued or running.
    pub live: u32,
    pub owner: String,
    /// The bytes it has written over its life.
    pub wrote: u64,
    pub counts: Counts,
    /// Since its owner last asked anything of it.
    pub idle_ms: u64,
}

/// A lane as its owner sees it, written for an agent to act on: where it is, what was asked
/// of it, and how each job ended.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    pub row: Row,
    /// Its jobs: the last that ended, oldest first, then the live ones.
    pub jobs: Vec<JobRow>,
    pub root: ContentId,
    /// Its last roots, oldest first, the one it is at last: what a job may be asked of.
    pub roots: Vec<ContentId>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct JobRow {
    pub at: ContentId,
    pub job: u64,
    pub kind: Kind,
    pub task: String,
    pub stage: Stage,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum Stage {
    Queued,
    Running { warmth: Warmth, machine: String },
    Ended(Outcome),
}

/// The hub as a whole.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Status {
    /// Jobs queued or running.
    pub live: u32,
    pub bases: u32,
    pub lanes: u32,
    /// Every lane's counts together, the lanes since closed included.
    pub counts: Counts,
    pub schema: u32,
}
