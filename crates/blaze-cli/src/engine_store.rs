//! The puts of `blaze engine-check` (engine.md E6; "The design" 5): after each phase's merge,
//! the records its tasks made go to the store in bulk, sorted by key (rule 25), so the next
//! phase's tasks read them through the store as the old functions expect (`work::payload`).
//!
//! A record is put in the engine's form: one whole record (blaze-drive record.rs
//! `write_plain`) holding its name, source, out, read list and payload, under its task's key.
//! Its findings are in the engine's answer rows, not in the record (the persisted answer is the
//! next piece). The keys are the engine's own ([`tools`]): another form of record under its own
//! keys, so the old machine's record of one task and the engine's never meet under one key
//! (rule 21).

use blaze_drive::{Key, Name, Tool};
use blaze_engine::Tools;
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::CliError;
use crate::store::Store;

/// The domain of the engine's per-kind tools: every kind's tool (home.rs `tools`) hashed under
/// it, so an edit of one producer still moves only its kinds' keys (rule 52).
const ENGINE_FORM: &str = "blaze-cli engine record form 1";

/// The engine's tool of each kind, from the kinds' tools (home.rs `tools`).
pub(crate) fn tools(kinds: &[Tool; blaze_drive::key::KINDS]) -> Tools {
    Tools(kinds.map(|tool| Tool(ContentId::of(ENGINE_FORM, &tool.0.0))))
}

const RECORD_FORM: &str = "blaze-cli engine record key 2";

/// The store key of the record a task's entry made: its task key (text, tool) and the record's
/// own bytes (out, read list, payload), so two entries of one task that made other bytes write
/// under two keys and two values never meet under one (rule 21), whatever the payload read that
/// its cells do not say. The stored answer keeps each row's key (pages.rs `records`), so a
/// request reads the record of a row that stands by it.
pub(crate) fn record_key(key: Key, out: ContentId, reads: &[(Name, ContentId)], payload: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(RECORD_FORM);
    h.update(&key.0.0).update(&out.0).update(&(reads.len() as u64).to_le_bytes());
    for (read, digest) in reads.iter() {
        h.update(&[read.kind as u8]).update(&read.subject.0).update(&digest.0);
    }
    h.update(&(payload.len() as u64).to_le_bytes()).update(payload);
    Key(h.finish())
}

/// One record a task of the merged phase made, to put under its task's key.
pub(crate) struct Put<'r> {
    pub(crate) key: Key,
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) out: ContentId,
    pub(crate) reads: &'r [(Name, ContentId)],
    pub(crate) payload: &'r [u8],
}

/// Puts every record of `puts` in key order (rule 25), each written once into `alloc` and
/// handed to the store; returns how many.
pub(crate) fn put_all(cx: &Cx, store: &mut dyn Store<'_>, puts: &mut [Put<'_>], alloc: blaze_own::Alloc<'_>) -> Result<u32, CliError> {
    let cx = cx.span("engine: a phase's records put");
    puts.sort_unstable_by_key(|put| put.key);
    for put in puts.iter() {
        let bytes = blaze_drive::record::write_plain(put.name, put.source, put.out, put.reads, put.payload, alloc);
        store.put(&cx, put.key, &bytes)?;
    }
    Ok(u32::try_from(puts.len()).unwrap_or(u32::MAX))
}
