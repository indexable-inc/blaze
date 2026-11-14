//! The remote-execution protocol: types and their wire form (decision 16). A remote worker is
//! the same task code on another machine, reading nothing but the records its action names.
//!
//! The worker contract:
//! 1. A [`Batch`] arrives: the namespace, the toolchain, and actions. Each action is the core's
//!    [`Action`] (the record it makes, the records it may read).
//! 2. The worker pulls each action's inputs from the bucket (verified like any pull) and runs
//!    the task with an [`Inputs`] reader holding those records and nothing else. A read of
//!    anything else is the core's [`NotAnInput`], and the action fails with it.
//! 3. The worker writes all results of the batch as segments of the batch's namespace, each
//!    with a statement signed by its builder key (role build), as a push does: each record made
//!    is filed under its own kind and key (the action's `made`). It answers an [`Outcome`] per
//!    action: the segments holding its records, its logs and its usage. The outcome is a record
//!    of the core's `ActionResult` kind, keyed by the toolchain, the record made and the inputs
//!    ([`Outcome::record`]).
//! 4. The results must be byte-identical to a local run of the same action (rule
//!    deterministic-output). The simulation checks it, and a push's determinism check compares
//!    them to what a second builder made.
//!
//! Where an action runs is the driver's rule, not this crate's
//! (`blaze_keyed::driver::stays_local`): under [`blaze_keyed::exec::LOCAL_TASKS`] misses
//! everything stays local, and cold fan-out goes remote in batches.
//!
//! Wire form of a batch, little-endian: `BLRA`, format u32, namespace u64, toolchain digest 32,
//! actions u32, zero u32. Per action: made (tag u16, zero u16, key 32), inputs u32, then inputs
//! of 36 bytes each (tag u16, zero u16, key 32). An outcome per action: `BLRO`, format u32,
//! action key 32, status u32 (0 made, 1 not an input, 2 failed), segments u32, log length u32,
//! zero u32, cpu ns u64, peak bytes u64, then segments of 56 bytes (as a bundle's), the log,
//! and for status 1 the record asked (36 bytes).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::BTreeMap;

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;
pub use blaze_keyed::exec::{Action, NotAnInput, RecordRef};
use blaze_keyed::{
    kind::{ActionInputs, ActionResult, Kind, Tag, ToolId},
    store::StoreError,
};

use crate::{
    bundle::SEGMENTS_MOST,
    segment::SegmentRef,
    wire::{Bounded, id_at, put_u32, put_u64, u32_at, u64_at},
};

const ROW: usize = SegmentRef::ROW;

/// The most actions a batch holds.
pub const ACTIONS_MOST: usize = 4096;
/// The most inputs an action names.
pub const INPUTS_MOST: usize = 1 << 16;
/// The most bytes a batch may be.
pub const BATCH_MOST: u64 = 64 << 20;
/// The most bytes an action's log may be.
pub const LOG_MOST: usize = 1 << 20;

/// An action as owned and sent: at most [`INPUTS_MOST`] inputs, by construction.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sent {
    pub made: RecordRef,
    pub inputs: Bounded<RecordRef, INPUTS_MOST>,
}

impl Sent {
    pub fn as_action(&self) -> Action<'_> {
        Action { made: self.made, inputs: &self.inputs }
    }

    /// Its bytes on the wire: made (36), inputs u32, inputs (36 each).
    fn wire_len(&self) -> u64 {
        40 + 36 * self.inputs.len() as u64
    }
}

/// A batch: at most [`ACTIONS_MOST`] actions and [`BATCH_MOST`] bytes encoded. Made only by
/// [`Batch::new`], so every batch that exists decodes back to itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Batch {
    namespace: Namespace,
    toolchain: ContentId,
    actions: Vec<Sent>,
}

fn put_ref(out: &mut Vec<u8>, it: &RecordRef) {
    out.extend_from_slice(&it.tag.0.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&it.key.0);
}

fn ref_at(bytes: &[u8], at: usize) -> Option<RecordRef> {
    let field = bytes.get(at..at + 36)?;
    (field[2..4] == [0, 0]).then(|| RecordRef { tag: Tag(u16::from_le_bytes([field[0], field[1]])), key: id_at(field, 4) })
}

impl Batch {
    /// The batch, or why not: more actions than [`ACTIONS_MOST`] or more bytes encoded than
    /// [`BATCH_MOST`], the bounds its decoder holds it to.
    pub fn new(namespace: Namespace, toolchain: ContentId, actions: Vec<Sent>) -> Result<Batch, &'static str> {
        if actions.len() > ACTIONS_MOST {
            return Err("a batch of more actions than a batch holds");
        }
        if 56 + actions.iter().map(Sent::wire_len).sum::<u64>() > BATCH_MOST {
            return Err("a batch larger than a batch may be");
        }
        Ok(Batch { namespace, toolchain, actions })
    }

    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    pub fn toolchain(&self) -> ContentId {
        self.toolchain
    }

    pub fn actions(&self) -> &[Sent] {
        &self.actions
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"BLRA");
        put_u32(&mut out, 1);
        put_u64(&mut out, self.namespace.0);
        out.extend_from_slice(&self.toolchain.0);
        put_u32(&mut out, self.actions.len() as u32);
        put_u32(&mut out, 0);
        for action in &self.actions {
            put_ref(&mut out, &action.made);
            put_u32(&mut out, action.inputs.len() as u32);
            for input in &action.inputs {
                put_ref(&mut out, input);
            }
        }
        out
    }

    /// The batch in `bytes`, within its bounds. Why not, as a fixed phrase.
    pub fn decode(bytes: &[u8]) -> Result<Batch, &'static str> {
        if bytes.len() as u64 > BATCH_MOST {
            return Err("a batch larger than a batch may be");
        }
        if bytes.len() < 56 || bytes[0..4] != *b"BLRA" || u32_at(bytes, 4) != 1 {
            return Err("not a batch of format 1");
        }
        let count = u32_at(bytes, 48) as usize;
        if count > ACTIONS_MOST {
            return Err("a batch of more actions than a batch holds");
        }
        let mut at = 56;
        let mut actions = Vec::with_capacity(count);
        for _ in 0..count {
            let made = ref_at(bytes, at).ok_or("a batch cut short")?;
            let n = bytes.get(at + 36..at + 40).map(|it| u32_at(it, 0) as usize).ok_or("a batch cut short")?;
            if n > INPUTS_MOST || n > (bytes.len() - at - 40) / 36 {
                return Err("an action of more inputs than an action names or the batch holds");
            }
            at += 40;
            let mut inputs = Vec::with_capacity(n);
            for _ in 0..n {
                inputs.push(ref_at(bytes, at).ok_or("a batch cut short")?);
                at += 36;
            }
            let inputs = Bounded::new(inputs).map_err(|_| "an action of more inputs than an action names")?;
            actions.push(Sent { made, inputs });
        }
        if at != bytes.len() {
            return Err("a batch with bytes past its last action");
        }
        Batch::new(Namespace(u64_at(bytes, 8)), id_at(bytes, 16), actions)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Usage {
    pub cpu_ns: u64,
    pub peak_bytes: u64,
}

/// What became of one action.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    /// Its records are in these segments.
    Made(Bounded<SegmentRef, SEGMENTS_MOST>),
    NotAnInput(NotAnInput),
    Failed,
}

/// An action's outcome; every bounded field is bounded by its type, so it decodes back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    pub action: ContentId,
    pub status: Status,
    pub logs: Bounded<u8, LOG_MOST>,
    pub usage: Usage,
}

impl Outcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"BLRO");
        put_u32(&mut out, 1);
        out.extend_from_slice(&self.action.0);
        let (status, segments): (u32, &[SegmentRef]) = match &self.status {
            Status::Made(segments) => (0, segments),
            Status::NotAnInput(_) => (1, &[]),
            Status::Failed => (2, &[]),
        };
        put_u32(&mut out, status);
        put_u32(&mut out, segments.len() as u32);
        put_u32(&mut out, self.logs.len() as u32);
        put_u32(&mut out, 0);
        put_u64(&mut out, self.usage.cpu_ns);
        put_u64(&mut out, self.usage.peak_bytes);
        for it in segments {
            out.extend_from_slice(&it.encode());
        }
        out.extend_from_slice(&self.logs);
        if let Status::NotAnInput(it) = &self.status {
            put_ref(&mut out, &it.asked);
        }
        out
    }

    /// The outcome in `bytes` for the action making `action`.
    pub fn decode(bytes: &[u8], action: RecordRef) -> Result<Outcome, &'static str> {
        if bytes.len() < 72 || bytes[0..4] != *b"BLRO" || u32_at(bytes, 4) != 1 {
            return Err("not an outcome of format 1");
        }
        if id_at(bytes, 8) != action.key {
            return Err("an outcome for another action");
        }
        let (status, segments, log) = (u32_at(bytes, 40), u32_at(bytes, 44) as usize, u32_at(bytes, 48) as usize);
        if log > LOG_MOST || segments > SEGMENTS_MOST || u32_at(bytes, 52) != 0 {
            return Err("an outcome past its bounds");
        }
        if status != 0 && segments != 0 {
            return Err("an outcome naming segments for an action that made nothing");
        }
        let mut at = 72;
        let mut refs = Vec::with_capacity(segments);
        for _ in 0..segments {
            let row = bytes.get(at..at + ROW).ok_or("an outcome cut short")?;
            refs.push(SegmentRef::decode(row).map_err(|_| "an outcome naming a malformed segment")?);
            at += ROW;
        }
        let logs = bytes.get(at..at + log).ok_or("an outcome cut short")?.to_vec();
        let logs = Bounded::new(logs).map_err(|_| "an outcome past its bounds")?;
        at += log;
        let refs = Bounded::new(refs).map_err(|_| "an outcome past its bounds")?;
        let status = match status {
            0 => Status::Made(refs),
            1 => {
                let asked = ref_at(bytes, at).ok_or("an outcome cut short")?;
                at += 36;
                Status::NotAnInput(NotAnInput { asked, action })
            }
            2 => Status::Failed,
            _ => return Err("an outcome of an unknown status"),
        };
        if at != bytes.len() {
            return Err("an outcome with bytes past its end");
        }
        Ok(Outcome { action: action.key, status, logs, usage: Usage { cpu_ns: u64_at(bytes, 56), peak_bytes: u64_at(bytes, 64) } })
    }

    /// The outcome as the record the store files it under: of the core's `ActionResult` kind,
    /// keyed by `tool`, the record `sent` makes and its inputs; its bytes this outcome's
    /// encoding. Filed once its segments are known (the outcome names them, so it is never in
    /// the segment of the records it names).
    pub fn record(&self, tool: ToolId, sent: &Sent) -> (RecordRef, Vec<u8>) {
        let key = <ActionResult as Kind>::key(&ActionInputs { tool, made: sent.made, inputs: &sent.inputs });
        (RecordRef::of(&key), self.encode())
    }
}

/// What a task reads: its action's inputs and nothing else.
pub struct Inputs<'a> {
    action: Action<'a>,
    records: &'a BTreeMap<RecordRef, Vec<u8>>,
}

impl<'a> Inputs<'a> {
    /// `records` must hold every input of `action`; it may hold more, never readable here.
    pub fn new(action: Action<'a>, records: &'a BTreeMap<RecordRef, Vec<u8>>) -> Inputs<'a> {
        Inputs { action, records }
    }

    pub fn read(&self, asked: RecordRef) -> Result<&'a [u8], NotAnInput> {
        let refused = NotAnInput { asked, action: self.action.made };
        if !self.action.inputs.contains(&asked) {
            return Err(refused);
        }
        self.records.get(&asked).map(Vec::as_slice).ok_or(refused)
    }
}

/// A task: an action's records made from its inputs.
pub type Task<'t> = &'t dyn Fn(&Inputs<'_>) -> Result<Vec<u8>, NotAnInput>;

/// Runs a batch's actions with `task`, each over its own inputs. Each record made is filed under
/// its own kind and key (the action's `made`), all in one segment of the batch's namespace,
/// written by the store's own writer. With the segment it answers an outcome per action, the
/// segment's place left for the caller to fill once it is written. The same code a local run
/// uses. Why not, as the store's writer says it.
pub fn run(batch: &Batch, records: &BTreeMap<RecordRef, Vec<u8>>, task: Task<'_>) -> Result<(Vec<u8>, Vec<Outcome>), StoreError> {
    let mut made = Vec::new();
    let mut outcomes = Vec::with_capacity(batch.actions.len());
    for sent in &batch.actions {
        let inputs = Inputs::new(sent.as_action(), records);
        let status = match task(&inputs) {
            Ok(bytes) => {
                made.push((sent.made, bytes));
                Status::Made(Bounded::empty())
            }
            Err(not) => Status::NotAnInput(not),
        };
        outcomes.push(Outcome { action: sent.made.key, status, logs: Bounded::empty(), usage: Usage::default() });
    }
    let segment = crate::segment::make(batch.namespace, &made)?;
    Ok((segment, outcomes))
}
