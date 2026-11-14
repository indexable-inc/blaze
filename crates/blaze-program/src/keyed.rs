//! The one adapter between this crate and the core (`blaze_keyed`): which core kind each of this
//! crate's records is, how its keys stand in the core's `Key<K>`, and how its payloads sit in
//! the core's record framing (head, kind tag, version, full key, checked by
//! [`blaze_keyed::record::open`]). Each item here that bends or adds to a core type is a change
//! the core is to take (the lane's report lists them field by field); when it has, the item goes.
//!
//! - An instance's code is the core's [`Instance`] (an [`Opaque`] root, the payload
//!   [`InstanceCode`]'s). Its key is this crate's [`crate::key::instance_key`] (the plan key
//!   shared by both tiers, the tier byte, every inlinee's symbol and key), not the core's
//!   `InstanceInputs`, which has no field for the tier or the inlinees' symbols.
//! - A test's result is the core's [`TestRun`] (a `Traced` root: its outside reads as facts,
//!   the payload [`TestRecord`]'s). Its key is [`crate::reach::test_key`]'s.
//! - An instance's plan ([`InstancePlan`]) and a program's manifest ([`ProgramHead`] and its
//!   content-defined [`ProgramChunk`]s) are declared here: the core has no plan kind, and its
//!   `ProgramManifest` root (an image digest and a list of instance keys) cannot hold a
//!   manifest that is patched, not relinked.
//!
//! Every record here is write-once: a key names exactly one value. A program's manifest
//! changes with every edit, so it is filed by content ([`head_key`]); which manifest is a
//! program's current one is a root the driver keeps, never a record overwritten.

#![blaze::sans_io]

use std::alloc::Global;
use std::fmt;

use blaze_keyed::kind::{Grain, Instance, Keep, Key, Kind, Layout, Tag, TestRun, Trace};
use blaze_keyed::record::{self, Fact, FactKind, Opaque, RecordError, StableId};

use crate::codec::{CodecError, Digest, Hasher, In, Out, Payload};
use crate::key::{InstanceKey, PlanKey};
use crate::program::{CHUNK_DOMAIN, Chunks, Program};
use crate::record::{CodeHead, InstanceCode, Plan, TestKey, TestRecord};

/// Why a filed record is not the value asked for: the core refused its framing, or the payload
/// does not decode.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FiledError {
    Record(RecordError),
    Codec(CodecError),
    /// A manifest names a chunk the reader does not have.
    MissingChunk { chunk: Digest },
}

impl fmt::Display for FiledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FiledError::Record(error) => write!(f, "{error}"),
            FiledError::Codec(error) => write!(f, "{error}"),
            FiledError::MissingChunk { chunk } => {
                write!(f, "the program manifest names chunk {chunk}, which is not filed")
            }
        }
    }
}

impl From<RecordError> for FiledError {
    fn from(error: RecordError) -> FiledError {
        FiledError::Record(error)
    }
}

impl From<CodecError> for FiledError {
    fn from(error: CodecError) -> FiledError {
        FiledError::Codec(error)
    }
}

macro_rules! local_kind {
    ($(#[$meta:meta])* $name:ident { tag: $tag:literal, grain: $grain:ident, keep: $keep:ident }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct $name;

        impl Kind for $name {
            const TAG: Tag = Tag($tag);
            const NAME: &'static str = ::core::stringify!($name);
            const DOMAIN: &'static str = ::core::concat!("blaze-keyed ", ::core::stringify!($name), " key");
            const VERSION: u16 = 1;
            const LAYOUT: Layout = Layout::Flat;
            const GRAIN: Grain = Grain::$grain;
            const TRACE: Trace = Trace::Key;
            const KEEP: Keep = Keep::$keep;
            type Root = Opaque;
            type Inputs<'a> = Known<'a>;

            fn key(inputs: &Known<'_>) -> Key<$name> {
                Key::from_id(*inputs.0)
            }
        }
    };
}

/// The inputs of a kind declared here: its key, computed by this crate's key rule (the report
/// lists each rule for the core's `kind.rs`).
pub struct Known<'a>(pub &'a Digest);

local_kind! {
    /// An instance's plan: its calls and its own facts, keyed by the plan key (the body's key,
    /// the type arguments, the settings but the tier): one plan for both tiers. Not yet in the
    /// core's `TAGS`: the next free tag on 2026-11-13.
    InstancePlan { tag: 12, grain: Instance, keep: DiskAndResident }
}

local_kind! {
    /// One content-defined chunk of a program's manifest, keyed by its content.
    ProgramChunk { tag: 13, grain: Program, keep: Disk }
}

local_kind! {
    /// A program's manifest head: the header (machine, settings, roots, section lengths, the
    /// generation) and the chunk directory, keyed by its content ([`head_key`]).
    ProgramHead { tag: 14, grain: Program, keep: Disk }
}

fn opaque_payload<'b, K: Kind<Root = Opaque>>(bytes: &'b [u8], key: &Key<K>) -> Result<&'b [u8], FiledError> {
    let root = record::open::<K>(bytes, key)?;
    root.payload.follow(bytes).map_err(|error| FiledError::Record(RecordError::Format(error)))
}

/// The core key of the code of the instance keyed `key`.
pub fn code_key(key: &InstanceKey) -> Key<Instance> {
    Key::from_id(key.0)
}

/// The record filing `code` under `key`.
pub fn write_code(key: &InstanceKey, code: &InstanceCode) -> Vec<u8> {
    record::write_opaque(&code_key(key), &code.encode())
}

/// The payload of the code record `bytes` (16-aligned), its framing checked against `key`.
pub fn code_payload<'b>(bytes: &'b [u8], key: &InstanceKey) -> Result<&'b [u8], FiledError> {
    opaque_payload(bytes, &code_key(key))
}

pub fn read_code(bytes: &[u8], key: &InstanceKey) -> Result<InstanceCode, FiledError> {
    Ok(InstanceCode::decode(code_payload(bytes, key)?)?)
}

/// The head of the code record `bytes`, reading nothing of the payload after it.
pub fn read_code_head(bytes: &[u8], key: &InstanceKey) -> Result<CodeHead, FiledError> {
    Ok(CodeHead::decode_prefix(code_payload(bytes, key)?)?)
}

pub fn plan_key(key: &PlanKey) -> Key<InstancePlan> {
    InstancePlan::key(&Known(&key.0))
}

pub fn write_plan(key: &PlanKey, plan: &Plan) -> Vec<u8> {
    record::write_opaque(&plan_key(key), &plan.encode())
}

pub fn read_plan(bytes: &[u8], key: &PlanKey) -> Result<Plan, FiledError> {
    Ok(Plan::decode(opaque_payload(bytes, &plan_key(key))?)?)
}

/// The core key of the result of the test keyed `key`.
pub fn test_key(key: &TestKey) -> Key<TestRun> {
    Key::from_id(key.0)
}

/// What an absent outside read digests to in a fact: absence is a value (rule 21).
pub const ABSENT: Digest = Digest([0; 32]);

/// The record filing `result`: its outside reads as the core's facts (an outside read's fact
/// id is its bytes', [`FactKind::Late`]), the result as the payload.
pub fn write_test(result: &TestRecord) -> Vec<u8> {
    let mut reads = Vec::with_capacity_in(result.reads.len(), Global);
    for read in &result.reads {
        let id = record::fact_id(FactKind::Late, &StableId::of(&read.what, 0), b"");
        reads.push(Fact { id, digest: read.digest.unwrap_or(ABSENT) });
    }
    record::write_traced(&test_key(&result.key), &reads, &[], &result.encode())
}

pub fn read_test(bytes: &[u8], key: &TestKey) -> Result<TestRecord, FiledError> {
    let core_key = test_key(key);
    let root = record::open::<TestRun>(bytes, &core_key)?;
    let payload = root.payload.follow(bytes).map_err(|error| FiledError::Record(RecordError::Format(error)))?;
    let result = TestRecord::decode(payload)?;
    if result.key != *key {
        return Err(FiledError::Record(RecordError::Key { found: result.key.0, wanted: key.0 }));
    }
    Ok(result)
}

/// The key a manifest head is filed under: the program's key, its header and its chunk
/// directory, so a manifest's bytes are one value under one key.
pub fn head_key(program: &Program, chunks: &Chunks) -> Key<ProgramHead> {
    let mut hasher = Hasher::new("blaze program manifest head 1");
    hasher.id(&program.key()).part(&chunks.header);
    for digest in chunks.directory() {
        hasher.id(&digest);
    }
    ProgramHead::key(&Known(&hasher.finish()))
}

/// The records a program's manifest is filed as: its head under [`head_key`], then each chunk
/// under its content's digest (a chunk an earlier manifest filed is the same bytes again, a
/// [`blaze_keyed::store::Put::Same`]).
pub fn write_program(program: &Program) -> (Key<ProgramHead>, Vec<u8>, Vec<(Key<ProgramChunk>, Vec<u8>)>) {
    let chunks = program.chunks();
    let key = head_key(program, &chunks);
    let directory = chunks.directory();
    let mut head = Out::new();
    head.bytes(&chunks.header);
    head.count(directory.len());
    for digest in &directory {
        head.digest(digest);
    }
    let head = record::write_opaque(&key, &head.finish());
    let mut filed = Vec::with_capacity_in(chunks.chunks.len(), Global);
    for (digest, chunk) in directory.iter().zip(&chunks.chunks) {
        let chunk_key = ProgramChunk::key(&Known(digest));
        filed.push((chunk_key, record::write_opaque(&chunk_key, chunk)));
    }
    (key, head, filed)
}

/// The program the head record `bytes` under `key` holds, each chunk read by `chunk` (its key,
/// giving its 16-aligned record) and checked against its digest.
pub fn read_program<'c>(
    bytes: &[u8],
    key: &Key<ProgramHead>,
    mut chunk: impl FnMut(&Key<ProgramChunk>) -> Option<&'c [u8]>,
) -> Result<Program, FiledError> {
    let payload = opaque_payload(bytes, key)?;
    let mut input = In::new("blaze program manifest head", payload);
    let header = input.bytes_owned()?;
    let count = input.count(32)?;
    let mut chunks = Chunks { header, chunks: Vec::with_capacity_in(count, Global) };
    for _ in 0..count {
        let digest = input.digest()?;
        let chunk_key = ProgramChunk::key(&Known(&digest));
        let Some(record) = chunk(&chunk_key) else { return Err(FiledError::MissingChunk { chunk: digest }) };
        let body = opaque_payload(record, &chunk_key)?;
        if Digest::of(CHUNK_DOMAIN, body) != digest {
            return Err(FiledError::Record(RecordError::Key { found: Digest::of(CHUNK_DOMAIN, body), wanted: digest }));
        }
        chunks.chunks.push(body.to_vec());
    }
    input.end()?;
    Ok(Program::from_chunks(&chunks)?)
}
