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
//!    with a statement signed by its builder key (role build), as a push does, and answers an
//!    [`Outcome`] per action: the segments holding its records, its logs and its usage.
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
use blaze_keyed::kind::Tag;

use crate::{
    bundle::SegmentRef,
    wire::{id_at, put_u32, put_u64, u32_at, u64_at},
};

/// The most actions a batch holds.
pub const ACTIONS_MOST: usize = 4096;
/// The most inputs an action names.
pub const INPUTS_MOST: usize = 1 << 16;
/// The most bytes a batch may be.
pub const BATCH_MOST: u64 = 64 << 20;
/// The most bytes an action's log may be.
pub const LOG_MOST: u32 = 1 << 20;

/// An action as owned and sent.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sent {
    pub made: RecordRef,
    pub inputs: Vec<RecordRef>,
}

impl Sent {
    pub fn as_action(&self) -> Action<'_> {
        Action { made: self.made, inputs: &self.inputs }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Batch {
    pub namespace: Namespace,
    pub toolchain: ContentId,
    pub actions: Vec<Sent>,
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
            if n > INPUTS_MOST {
                return Err("an action of more inputs than an action names");
            }
            at += 40;
            let mut inputs = Vec::with_capacity(n);
            for _ in 0..n {
                inputs.push(ref_at(bytes, at).ok_or("a batch cut short")?);
                at += 36;
            }
            actions.push(Sent { made, inputs });
        }
        if at != bytes.len() {
            return Err("a batch with bytes past its last action");
        }
        Ok(Batch { namespace: Namespace(u64_at(bytes, 8)), toolchain: id_at(bytes, 16), actions })
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
    Made(Vec<SegmentRef>),
    NotAnInput(NotAnInput),
    Failed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    pub action: ContentId,
    pub status: Status,
    pub logs: Vec<u8>,
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
            out.extend_from_slice(&it.digest.0);
            put_u64(&mut out, it.len);
            put_u64(&mut out, it.index_at);
            put_u32(&mut out, it.count);
            put_u32(&mut out, 0);
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
        let (status, segments, log) = (u32_at(bytes, 40), u32_at(bytes, 44) as usize, u32_at(bytes, 48));
        if log > LOG_MOST || segments > crate::bundle::SEGMENTS_MOST {
            return Err("an outcome past its bounds");
        }
        let mut at = 72;
        let mut refs = Vec::with_capacity(segments);
        for _ in 0..segments {
            let row = bytes.get(at..at + 56).ok_or("an outcome cut short")?;
            refs.push(SegmentRef { digest: id_at(row, 0), len: u64_at(row, 32), index_at: u64_at(row, 40), count: u32_at(row, 48) });
            at += 56;
        }
        let logs = bytes.get(at..at + log as usize).ok_or("an outcome cut short")?.to_vec();
        at += log as usize;
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

/// Runs a batch's actions with `task`, each over its own inputs. The records made (key, kind
/// byte, bytes) go into one segment of the batch's namespace. With the segment it answers an
/// outcome per action, the segment's place left for the caller to fill once it is written.
/// The same code a local run uses.
pub fn run(batch: &Batch, records: &BTreeMap<RecordRef, Vec<u8>>, task: Task<'_>) -> (Vec<u8>, Vec<Outcome>) {
    let mut made = Vec::new();
    let mut outcomes = Vec::with_capacity(batch.actions.len());
    for sent in &batch.actions {
        let inputs = Inputs::new(sent.as_action(), records);
        let status = match task(&inputs) {
            Ok(bytes) => {
                made.push((sent.made.key.0, sent.made.tag.0 as u8, bytes));
                Status::Made(Vec::new())
            }
            Err(not) => Status::NotAnInput(not),
        };
        outcomes.push(Outcome { action: sent.made.key, status, logs: Vec::new(), usage: Usage::default() });
    }
    let segment = crate::segment::make(batch.namespace, &made);
    (segment, outcomes)
}
