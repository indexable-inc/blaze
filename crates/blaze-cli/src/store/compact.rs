//! When a layer's packs are merged (std.md, "End state: efficiency", part (e) and piece 8), and
//! the merge's typed refusal.
//!
//! The rule, after the answer, per layer (the checkout's, the public machine store, the user's
//! own): merge when the layer holds more than `packs_most` packs ([`super::PACKS_MOST`], 8), or
//! when more than one byte in [`DEAD_SHARE`] of its payload bytes is a second copy of a key
//! another pack of the layer holds (two commands sealing one record at once; a merge's crash
//! leftover, which keeps old and new packs both). Both are known from the indexes the open
//! read: no payload is read to decide. The merge itself is `PackStore::merge`: a streamed copy
//! record by record through one piece of `chunks::PART` bytes, a gathered publish of file
//! ranges, the new pack renamed in before any old one is removed, so a reader holding an old
//! pack keeps reading it (an open file outlives its name) and one that lost its name answers a
//! miss, never other bytes.
//!
//! What this rule cannot see, by name: a record of an OLD TOOL. Every key holds the tool
//! (`home::tool`: the binary's linker id and every form it writes; blaze-drive `task_key`), so
//! each rebuilt blaze writes the toolchain's records again under new keys: 27 records, 24 MB,
//! a pack per rebuild in `~/.blaze/v2/shared` (2026-11-16: ten such packs, sizes equal, keys
//! all different). Those records are live by key and dead by use: no binary that exists asks
//! for them. Removing them is a named piece: the tool in each pack's footer (a format bump),
//! and the rule "a pack of another tool is dead bytes", or keys of the toolchain's records by
//! the code identity of the stages that make them instead of the whole binary's.

use blaze_drive::Key;
use blaze_own::Vec;
use blaze_own::alloc_ref::SharedAlloc;

use super::chunks::PutError;
use super::{CliError, PackStore, RemoteError};

/// Past one byte in this many of a layer's payload bytes held twice, the layer is merged even
/// under `packs_most`.
pub(crate) const DEAD_SHARE: u64 = 4;

/// What the rule decided for a layer, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Compact {
    /// Few enough packs, few enough second copies.
    Not,
    /// More packs than the layer keeps.
    TooManyPacks { packs: usize, most: usize },
    /// `dead` of the layer's `all` payload bytes are second copies.
    Dead { dead: u64, all: u64 },
}

impl PackStore {
    /// The merge's typed refusal: no memory for a table of `rows` index rows over this store's
    /// packs, naming its folder (rule 3: nothing merged, every pack stays readable).
    pub(super) fn no_room(&self, rows: usize) -> CliError {
        let packs = u32::try_from(self.packs.len()).unwrap_or(u32::MAX);
        CliError::Remote(RemoteError::Put(PutError::MergeNoRoom { folder: self.folder, packs, rows: rows as u64 }))
    }

    /// The compaction rule (the module's doc) over the indexes this store holds.
    pub(crate) fn compaction(&self) -> Result<Compact, CliError> {
        let packs = self.packs.len();
        if packs > self.packs_most {
            return Ok(Compact::TooManyPacks { packs, most: self.packs_most });
        }
        if packs < 2 {
            return Ok(Compact::Not);
        }
        let all_rows: usize = self.packs.iter().map(|(_, pack)| pack.rows.len()).sum();
        let mut keys: Vec<(Key, u64), SharedAlloc<'static>> = Vec::try_with_capacity_in(all_rows, SharedAlloc::global()).map_err(|_| self.no_room(all_rows))?;
        let mut all = 0u64;
        for (_, pack) in self.packs.iter() {
            for (key, _, len) in pack.rows.iter() {
                let padded = u64::from(*len).next_multiple_of(8);
                all = all.saturating_add(padded);
                keys.try_push((*key, padded)).map_err(|_| self.no_room(all_rows))?;
            }
        }
        keys.sort_unstable_by_key(|row| row.0);
        let mut dead = 0u64;
        for pair in keys.windows(2) {
            if pair[0].0 == pair[1].0 {
                dead = dead.saturating_add(pair[1].1);
            }
        }
        if dead > 0 && dead.saturating_mul(DEAD_SHARE) > all {
            return Ok(Compact::Dead { dead, all });
        }
        Ok(Compact::Not)
    }
}
