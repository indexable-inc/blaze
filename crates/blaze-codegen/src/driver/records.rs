//! Describes as store records (docs/v2/plan/design/store.md, "Describe records"): the walk
//! asks the store for a function's describe before it asks the front end, and keeps each
//! describe the front end made as a record for the next database to find.
//!
//! A describe kept in memory ([`super::kept`]) names the front end's instances, which are ids
//! of one database: a new project in a warm daemon, a restart and a second home begin with
//! none and described everything the program reaches (14,000 functions of the standard
//! library for hello-world). A record is keyed by content
//! ([`Frontend::record_key`](crate::backend::Frontend::record_key)) and names what it refers
//! to by content, so any database of the same sources finds it.
//!
//! Nothing here decides what a build holds. A record is what the front end's describe gives
//! for the key's content, so the walk over records equals the walk over none whenever the
//! key covers every input of the describe (rule 21), which `driver/tests/recorded.rs` holds
//! the walk to, with a key that leaves an input out as its control.
//!
//! The store is read on whichever core visits the function, each read under its own guard
//! and done with the bytes before it returns: a record is decoded in place and nothing of
//! the store's is held past the call.

use blaze_cas::{Key, Kind};

use super::Put;
use crate::CodegenError;

/// The store as the walk's describes read it, on every core at once.
pub trait Records: Sync {
    /// Gives `read` the verified bytes under `key`, borrowed for the call, and says whether
    /// the store had any; `Err` names why what is there can't be read.
    // dyn: one call a function the walk has no kept describe of, against a store lookup
    fn read(&self, key: &Key, read: &mut dyn FnMut(&[u8])) -> Result<bool, String>;
}

impl Records for blaze_cas::Reader {
    fn read(&self, key: &Key, read: &mut dyn FnMut(&[u8])) -> Result<bool, String> {
        let guard = blaze_cas::enter();
        match self.get(&guard, key) {
            Ok(Some(entry)) => {
                read(entry.bytes);
                Ok(true)
            }
            Ok(None) => Ok(false),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// The kind a describe's record is kept as: a query's result.
pub const KIND: Kind = Kind::Query;

/// The records a build's describes made, sorted by key, each key once: what the build adds to
/// its puts. Two describes of one key that gave other bytes are the build's error naming the
/// key and both content IDs (rule 21): one of the two read an input the key leaves out.
pub(super) fn sorted(mut made: Vec<Put>) -> Result<Vec<Put>, CodegenError> {
    made.sort_unstable_by(|a, b| a.key.cmp(&b.key).then_with(|| a.bytes.cmp(&b.bytes)));
    // alloc: per build, the records it gives the caller to put
    let mut puts: Vec<Put> = Vec::with_capacity(made.len());
    for put in made {
        match puts.last() {
            Some(kept) if kept.key == put.key => {
                if kept.bytes != put.bytes {
                    return Err(super::error(
                        crate::InstanceKey(blaze_ids::ContentId::default()),
                        blaze_items::ItemKey(blaze_ids::ContentId::default()),
                        // alloc: an error
                        format!(
                            "two describes of this build have one record key {:x} and other \
                             bytes ({:x} and {:x}): the key leaves out an input one of them read",
                            put.key.0,
                            blaze_cas::content_of(&kept.bytes),
                            blaze_cas::content_of(&put.bytes),
                        ),
                    ));
                }
            }
            _ => puts.push(put),
        }
    }
    Ok(puts)
}
