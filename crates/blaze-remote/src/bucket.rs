//! The bucket as effects: what a machine asks ([`Op`]), what it is answered ([`Reply`]), and
//! the one loop that carries a machine's asks to a [`Bucket`] ([`drive`]). The machines own no
//! IO and no clock: the time is given with every step, and each ask carries its own deadline.
//!
//! Requests are never retried. A failed or timed-out request ends the machine with a typed
//! [`Failure`] naming the object. The one loop that asks again is the root's compare-and-swap,
//! when another pusher moved the root, and it is bounded ([`crate::push::ROOT_TRIES`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

use crate::trust::Refusal;

/// Names one ask of a machine.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ticket(pub u64);

/// A put's precondition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cond {
    /// None.
    Any,
    /// The object does not exist (`If-None-Match: *`).
    Absent,
    /// The object's etag is this (`If-Match`).
    Matches(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// The object, or `len` bytes of it at `at`. More than `most` bytes is a failure.
    Get { name: String, range: Option<(u64, u64)>, most: u64 },
    /// Whether the object exists, and its length.
    Head { name: String },
    Put { name: String, bytes: Vec<u8>, cond: Cond },
    /// Starts a multipart upload.
    Begin { name: String },
    Part { name: String, upload: String, number: u32, bytes: Vec<u8> },
    Finish { name: String, upload: String, parts: Vec<(u32, String)>, cond: Cond },
}

impl Op {
    pub fn name(&self) -> &str {
        match self {
            Op::Get { name, .. } | Op::Head { name } | Op::Put { name, .. } | Op::Begin { name } | Op::Part { name, .. } | Op::Finish { name, .. } => name,
        }
    }

    /// The bytes it sends.
    pub fn sends(&self) -> u64 {
        match self {
            Op::Put { bytes, .. } | Op::Part { bytes, .. } => bytes.len() as u64,
            Op::Get { .. } | Op::Head { .. } | Op::Begin { .. } | Op::Finish { .. } => 0,
        }
    }

    /// The bytes it may receive.
    pub fn receives(&self) -> u64 {
        match self {
            Op::Get { range: Some((_, len)), .. } => *len,
            Op::Get { most, .. } => *most,
            Op::Head { .. } | Op::Put { .. } | Op::Begin { .. } | Op::Part { .. } | Op::Finish { .. } => 0,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    Got { bytes: Vec<u8>, etag: String },
    Missing,
    Exists { len: u64 },
    /// A put, part or finish took effect.
    Done { etag: String },
    Begun { upload: String },
    /// A put's precondition did not hold.
    Precondition,
    /// The request failed: why, never holding a credential.
    Failed { why: String },
    /// No answer by the ask's deadline.
    TimedOut,
}

/// One ask: the op and the time (ms on the machine's clock) it must be answered by.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asked {
    pub ticket: Ticket,
    pub op: Op,
    pub deadline_ms: u64,
}

/// A request's base allowance.
pub const BASE_MS: u64 = 15_000;
/// The slowest transfer allowed: 1 MiB/s, so 1 KiB a millisecond.
pub const BYTES_PER_MS: u64 = 1 << 10;

/// The deadline of `op` asked at `now_ms`: [`BASE_MS`] plus its bytes at [`BYTES_PER_MS`].
pub fn deadline(now_ms: u64, op: &Op) -> u64 {
    now_ms + BASE_MS + (op.sends() + op.receives().min(4 << 30)) / BYTES_PER_MS
}

/// What a machine ended with when it did not succeed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    /// An object was refused.
    Refused(Refusal),
    /// The bucket did not answer a request for `object`: why.
    Unreachable { object: String, why: String },
    /// No answer for `object` by its deadline.
    TimedOut { object: String },
    /// An object a signed object names is not in the bucket.
    Missing { object: String },
    /// The root moved under each of `tries` attempts.
    Contended { root: String, tries: u32 },
    /// Without compare-and-swap, the root read back is not the one put.
    LostRace { root: String },
    /// What a command needs and is not set: each by its setting's name.
    NotConfigured { missing: Vec<String> },
    /// A sink refused what was handed to it.
    Sink(String),
    /// A machine or driver was used against its contract.
    Misused(String),
}

impl From<Refusal> for Failure {
    fn from(refusal: Refusal) -> Failure {
        Failure::Refused(refusal)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Refused(refusal) => write!(f, "{refusal}"),
            Failure::Unreachable { object, why } => write!(f, "{object}: the bucket did not answer: {why} (not asked again)"),
            Failure::TimedOut { object } => write!(f, "{object}: no answer by its deadline (not asked again)"),
            Failure::Missing { object } => write!(f, "{object}: named by a signed object but not in the bucket"),
            Failure::Contended { root, tries } => write!(f, "{root}: another pusher moved the root under each of {tries} attempts"),
            Failure::LostRace { root } => write!(f, "{root}: the root read back is another pusher's (the bucket has no compare-and-swap); this push's run is not indexed, push again"),
            Failure::NotConfigured { missing } => write!(f, "not configured: {}", missing.join(", ")),
            Failure::Sink(why) => write!(f, "what was fetched was not taken: {why}"),
            Failure::Misused(why) => write!(f, "misused: {why}"),
        }
    }
}

/// A failure for a reply that is not a success to `asked`.
pub(crate) fn failed(object: &str, reply: Reply) -> Failure {
    match reply {
        Reply::Failed { why } => Failure::Unreachable { object: object.to_owned(), why },
        Reply::TimedOut => Failure::TimedOut { object: object.to_owned() },
        Reply::Missing => Failure::Missing { object: object.to_owned() },
        other => Failure::Misused(format!("{object}: an answer of the wrong kind: {}", kind(&other))),
    }
}

fn kind(reply: &Reply) -> &'static str {
    match reply {
        Reply::Got { .. } => "got",
        Reply::Missing => "missing",
        Reply::Exists { .. } => "exists",
        Reply::Done { .. } => "done",
        Reply::Begun { .. } => "begun",
        Reply::Precondition => "precondition failed",
        Reply::Failed { .. } => "failed",
        Reply::TimedOut => "timed out",
    }
}

/// A machine's step.
#[derive(Debug)]
pub enum Step<T> {
    /// These asks, and then the next replies (possibly none new, when it waits on earlier ones).
    Ask(Vec<Asked>),
    Done(Result<T, Failure>),
}

/// A sans-IO machine over a bucket.
pub trait Machine {
    type Out;
    /// What it hands on as it goes (a verified segment), taken by the driver's sink.
    type Item;

    fn step(&mut self, now_ms: u64, replies: Vec<(Ticket, Reply)>) -> Step<Self::Out>;

    /// The next item ready to hand on.
    fn take(&mut self) -> Option<Self::Item> {
        None
    }
}

/// Requests and bytes, both ways.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Counts {
    pub requests: u64,
    pub sent: u64,
    pub received: u64,
}

/// Something that carries asks to a bucket: the HTTP driver, or the fake in tests.
pub trait Bucket {
    /// Its clock, in ms.
    fn now_ms(&mut self) -> u64;
    fn send(&mut self, asked: Vec<Asked>);
    /// Asks not yet answered.
    fn in_flight(&self) -> usize;
    /// Blocks until at least one ask is answered (or its deadline passes, answered
    /// [`Reply::TimedOut`]); every answer ready.
    fn wait(&mut self) -> Vec<(Ticket, Reply)>;
    fn counts(&self) -> Counts;
}

/// The most steps a machine may take: past it, it is not making progress.
pub const STEPS_MOST: u64 = 1 << 24;

/// Drives `machine` on `bucket` to its end, handing each item to `sink`.
pub fn drive<M: Machine>(machine: &mut M, bucket: &mut dyn Bucket, sink: &mut dyn FnMut(M::Item) -> Result<(), String>) -> Result<M::Out, Failure> {
    let mut replies = Vec::new();
    for _ in 0..STEPS_MOST {
        let now = bucket.now_ms();
        let step = machine.step(now, std::mem::take(&mut replies));
        while let Some(item) = machine.take() {
            sink(item).map_err(Failure::Sink)?;
        }
        match step {
            Step::Done(out) => {
                // What a failed machine still has in flight is answered (by its deadline at
                // the latest) and dropped, so the next machine on this bucket never sees it.
                while bucket.in_flight() > 0 {
                    drop(bucket.wait());
                }
                return out;
            }
            Step::Ask(asked) => bucket.send(asked),
        }
        if bucket.in_flight() == 0 {
            return Err(Failure::Misused("a machine waits with nothing asked".to_owned()));
        }
        replies = bucket.wait();
    }
    Err(Failure::Misused(format!("a machine took {STEPS_MOST} steps without ending")))
}

/// Hands out tickets and remembers what each was for.
#[derive(Debug)]
pub(crate) struct Pending<P> {
    next: u64,
    asked: std::collections::BTreeMap<u64, P>,
}

impl<P> Pending<P> {
    pub(crate) fn new() -> Pending<P> {
        Pending { next: 0, asked: std::collections::BTreeMap::new() }
    }

    pub(crate) fn ask(&mut self, out: &mut Vec<Asked>, now_ms: u64, op: Op, what: P) {
        let ticket = Ticket(self.next);
        self.next += 1;
        self.asked.insert(ticket.0, what);
        out.push(Asked { ticket, deadline_ms: deadline(now_ms, &op), op });
    }

    pub(crate) fn take(&mut self, ticket: Ticket) -> Result<P, Failure> {
        self.asked.remove(&ticket.0).ok_or_else(|| Failure::Misused(format!("an answer to ticket {} that was not asked or was answered", ticket.0)))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.asked.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.asked.len()
    }
}
