//! The one adapter between this crate and the core (`blaze_keyed`): which core kind each of this
//! crate's records is, and how its payloads sit in the core's record framing. Every kind is the
//! core's, declared in its `kind.rs` with its key rule; nothing here declares a kind, a tag or a
//! key rule of its own (rule 30).
//!
//! - An instance's code is the core's [`Instance`] (an [`Opaque`] root, the payload
//!   [`InstanceCode`]'s), keyed by [`crate::key::instance_key`], which is the core's
//!   [`blaze_keyed::kind::InstanceInputs`] key: the plan key, the tier, every inlinee's symbol
//!   and instance key.
//! - An instance's plan is the core's [`InstancePlan`], keyed by [`crate::key::plan_key`] (the
//!   core's [`blaze_keyed::kind::PlanInputs`]).
//! - A test's result is the core's `TestRun`, an outside record whose base key is
//!   [`crate::reach::test_key`] (the core's [`blaze_keyed::kind::TestInputs`]); its shapes,
//!   values and the suite index are blaze-testsel's.
//! - A program's manifest is the core's [`ProgramManifest`] (its head: the header and the chunk
//!   directory, keyed by content, [`head_key`]) and its content-defined [`ProgramChunk`]s.
//!
//! Every reader takes the core's proof that the record is the one of its key ([`Holds`], made by
//! [`blaze_keyed::proof::hit`], the one check) and reads the payload through the core's one
//! payload reader ([`proof::payload`]), checking nothing again (decision 18).
//!
//! Every record here is write-once: a key names exactly one value. A program's manifest
//! changes with every edit, so it is filed by content ([`head_key`]); which manifest is a
//! program's current one is a root the driver keeps, never a record overwritten.

#![blaze::sans_io]

use std::alloc::Global;
use std::fmt;
use std::marker::PhantomData;

use blaze_keyed::kind::{ChunkInputs, Instance, InstancePlan, Key, Kind, ProgramChunk, ProgramInputs, ProgramManifest};
use blaze_keyed::proof::{self, Holds};
use blaze_keyed::record::{self, RecordError};

use crate::codec::{CodecError, Digest, In, Out, Payload};
use crate::key::{InstanceKey, PlanKey};
use crate::program::{CHUNK_DOMAIN, Chunks, Program};
use crate::record::{CodeHead, InstanceCode, Plan};

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

/// The core key of the code of the instance keyed `key`: the same id, computed by the core's
/// rule ([`crate::key::instance_key`]).
pub fn code_key(key: &InstanceKey) -> Key<Instance> {
    Key::from_id(key.0)
}

/// The record filing `code` under `key`.
pub fn write_code(key: &InstanceKey, code: &InstanceCode) -> Vec<u8> {
    record::write_opaque(&code_key(key), &code.encode())
}

/// The payload of the code record `bytes` (16-aligned) its proof `holds` is of.
pub fn code_payload<'b>(holds: &Holds<'_, Instance>, bytes: &'b [u8]) -> Result<&'b [u8], FiledError> {
    Ok(proof::payload(holds, bytes)?)
}

pub fn read_code(holds: &Holds<'_, Instance>, bytes: &[u8]) -> Result<InstanceCode, FiledError> {
    Ok(InstanceCode::decode(code_payload(holds, bytes)?)?)
}

/// The head of the code record `bytes`, reading nothing of the payload after it.
pub fn read_code_head(holds: &Holds<'_, Instance>, bytes: &[u8]) -> Result<CodeHead, FiledError> {
    Ok(CodeHead::decode_prefix(code_payload(holds, bytes)?)?)
}

/// The core key of the plan keyed `key`: the same id, computed by the core's rule
/// ([`crate::key::plan_key`]).
pub fn plan_key(key: &PlanKey) -> Key<InstancePlan> {
    Key::from_id(key.0)
}

pub fn write_plan(key: &PlanKey, plan: &Plan) -> Vec<u8> {
    record::write_opaque(&plan_key(key), &plan.encode())
}

pub fn read_plan(holds: &Holds<'_, InstancePlan>, bytes: &[u8]) -> Result<Plan, FiledError> {
    Ok(Plan::decode(proof::payload(holds, bytes)?)?)
}

/// The key a manifest head is filed under: the program's key, its header and its chunk
/// directory, so a manifest's bytes are one value under one key (the core's
/// [`ProgramInputs`]).
pub fn head_key(program: &Program, chunks: &Chunks) -> Key<ProgramManifest> {
    let directory = chunks.directory();
    ProgramManifest::key(&ProgramInputs { program: program.key(), header: &chunks.header, chunks: &directory })
}

/// The core key of a manifest's chunk whose content digests to `digest`.
pub fn chunk_key(digest: &Digest) -> Key<ProgramChunk> {
    ProgramChunk::key(&ChunkInputs { content: *digest, of: PhantomData })
}

/// The records a program's manifest is filed as: its head under [`head_key`], then each chunk
/// under its content's key (a chunk an earlier manifest filed is the same bytes again, a
/// [`blaze_keyed::store::Put::Same`]).
pub fn write_program(program: &Program) -> (Key<ProgramManifest>, Vec<u8>, Vec<(Key<ProgramChunk>, Vec<u8>)>) {
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
        let chunk_key = chunk_key(digest);
        filed.push((chunk_key, record::write_opaque(&chunk_key, chunk)));
    }
    (key, head, filed)
}

/// The program the head record `bytes` its proof `holds` is of holds, each chunk given by
/// `chunk` (its key, giving the core's proof of its record and the 16-aligned record). A chunk's
/// key is its content's, and its proof that the record is that key's: its bytes are not
/// digested again.
pub fn read_program<'s, 'c>(
    holds: &Holds<'s, ProgramManifest>,
    bytes: &[u8],
    mut chunk: impl FnMut(&Key<ProgramChunk>) -> Option<(Holds<'s, ProgramChunk>, &'c [u8])>,
) -> Result<Program, FiledError> {
    let payload = proof::payload(holds, bytes)?;
    let mut input = In::new("blaze program manifest head", payload);
    let header = input.bytes_owned()?;
    let count = input.count(32)?;
    let mut chunks = Chunks { header, chunks: Vec::with_capacity_in(count, Global) };
    for _ in 0..count {
        let digest = input.digest()?;
        let chunk_key = chunk_key(&digest);
        let Some((chunk_holds, record)) = chunk(&chunk_key) else {
            return Err(FiledError::MissingChunk { chunk: digest });
        };
        debug_assert_eq!(chunk_holds.key(), chunk_key, "the chunk given is the one asked");
        let body = proof::payload(&chunk_holds, record)?;
        debug_assert_eq!(Digest::of(CHUNK_DOMAIN, body), digest, "a chunk's record holds its content");
        chunks.chunks.push(body.to_vec());
    }
    input.end()?;
    Ok(Program::from_chunks(&chunks)?)
}
