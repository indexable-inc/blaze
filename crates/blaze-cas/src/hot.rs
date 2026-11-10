//! The store's side of its buffer pool (docs/v2/plan/design/store-pool.md, lane 4b): the pool's
//! reservation, the table of windows and the loads core, and which of the store's files are
//! open in them.
//!
//! A pack is opened in the pool with its blocks' checks as its tier opens, grows as a commit
//! makes more of it durable, and is retired with its file. Nothing reads through the pool yet:
//! the store still maps what it reads, and this is the state the reads move onto.
//!
//! Until the commit appends through the pool ([`crate::pool::Append`]), a commit's bytes reach
//! their pack by a write of the store's, not through the pack's window. So a page the
//! committed bytes ended in, if it is in memory, holds zeros where the commit's records now
//! are: growing a pack gives that one page back first ([`Pool::drop_stale`]), and its next
//! read loads it whole.
//!
//! Under the model checker (`cfg(blaze_check)`), which models the store's tables and segments
//! and no IO, there is no pool: the same calls do nothing.

use blaze_core::FileName;

#[cfg(not(blaze_check))]
use blaze_core::Cache;

#[cfg(not(blaze_check))]
use crate::pool::{FileId, Kernel, Loads, Opened, PAGE, Pool, Rank, WINDOW, WINDOWS, Windows};

/// A pack as the pool names it: its tier and its place in its tier's head.
#[cfg(not(blaze_check))]
fn pack_id(tier: usize, pack: usize) -> FileId {
    debug_assert!(tier < 1 << 8 && pack < crate::PACKS_MOST);
    FileId::Pack((tier << 16 | pack) as u32)
}

/// The pool, the windows of the files open in it, and the loads core that reads them.
#[cfg(not(blaze_check))]
struct Open {
    pool: Pool<Kernel>,
    windows: Windows,
    loads: Loads,
}

/// The store's hot tier.
pub(crate) struct Hot {
    #[cfg(not(blaze_check))]
    open: Result<Open, String>,
}

impl Hot {
    /// The pool reserved, with nothing open in it, its loads asking with tags
    /// `load_tags..load_tags + 2^32`. A reservation the system refused is kept as why
    /// ([`Hot::refused`]): the store stops, naming it, and nothing is read another way.
    #[cfg(not(blaze_check))]
    pub(crate) fn new(load_tags: u64) -> Hot {
        let open = Pool::new(WINDOWS, Kernel)
            .map(|pool| Open {
                pool,
                windows: Windows::new(WINDOWS),
                // Around the page cache: the pool is the one copy of what it reads.
                loads: Loads::new(load_tags, Cache::Bypassed),
            })
            // alloc: the error that stops the store
            .map_err(|err| err.to_string());
        Hot { open }
    }

    #[cfg(blaze_check)]
    pub(crate) fn new(_load_tags: u64) -> Hot {
        Hot {}
    }

    /// Why there is no pool, when its reservation was refused.
    pub(crate) fn refused(&self) -> Option<&str> {
        #[cfg(not(blaze_check))]
        let refused = self.open.as_ref().err().map(String::as_str);
        #[cfg(blaze_check)]
        let refused = None;
        refused
    }

    /// Pack `pack` of tier `tier`, the file `file`, commits `committed` bytes whose blocks'
    /// checks are `checks`: opened in the pool the first time, grown after.
    ///
    /// # Errors
    ///
    /// The pool has no windows left for it, the pack outgrew the windows it was opened with,
    /// or the page its committed bytes ended in is in use and cannot be given back: why, for
    /// the store to stop on, naming the file.
    #[cfg(not(blaze_check))]
    pub(crate) fn pack_committed(
        &mut self,
        tier: usize,
        pack: usize,
        file: &FileName,
        committed: u64,
        checks: &[u32],
    ) -> Result<(), String> {
        // clone: the error that stops the store
        let Open { pool, windows, loads } = self.open.as_mut().map_err(|why| why.clone())?;
        let id = pack_id(tier, pack);
        let Some(before) = windows.get(id).copied() else {
            let window = windows
                .open(pool, id, committed, Rank::Plain)
                // alloc: the error that stops the store
                .map_err(|err| err.to_string())?;
            // clone: the loads core keeps the name for its reads, and the checks to check them by
            loads.open(Opened { window, name: file.clone(), checks: checks.to_vec() });
            return Ok(());
        };
        if committed <= before.len {
            return Ok(());
        }
        if committed > u64::from(before.count) * WINDOW {
            // alloc: the error that stops the store
            return Err(format!(
                "it commits {committed} bytes, past the {} windows of the store's pool it was \
                 opened with at {} bytes",
                before.count, before.len
            ));
        }
        if before.len % PAGE != 0 {
            let page = before.first_page() + before.len / PAGE;
            if let Err(word) = pool.drop_stale(page) {
                // alloc: the error that stops the store
                return Err(format!(
                    "the page of the store's pool its committed bytes ended in (at {}) is {word:?} \
                     and cannot be given back for the commit that grew it",
                    before.len
                ));
            }
        }
        let window = windows.grow(id, committed);
        let known = (before.len / crate::disk::BLOCK) as usize;
        // alloc: the loads core keeps the new blocks' checks beside the rest
        loads.grow(window, checks[known..].to_vec());
        Ok(())
    }

    #[cfg(blaze_check)]
    pub(crate) fn pack_committed(
        &mut self,
        _tier: usize,
        _pack: usize,
        _file: &FileName,
        _committed: u64,
        _checks: &[u32],
    ) -> Result<(), String> {
        Ok(())
    }

    /// Pack `pack` of tier `tier` is removed: its pages given back and its windows freed. One
    /// never opened is nothing to do.
    ///
    /// # Errors
    ///
    /// A page of it is pinned or loading: why. Its windows stay taken.
    #[cfg(not(blaze_check))]
    pub(crate) fn pack_removed(&mut self, tier: usize, pack: usize) -> Result<(), String> {
        let Ok(Open { pool, windows, loads }) = self.open.as_mut() else { return Ok(()) };
        let id = pack_id(tier, pack);
        let Some(window) = windows.get(id).copied() else { return Ok(()) };
        // alloc: a problem reported
        windows.retire(pool, id).map_err(|err| err.to_string())?;
        loads.close(window.first);
        Ok(())
    }

    #[cfg(blaze_check)]
    pub(crate) fn pack_removed(&mut self, _tier: usize, _pack: usize) -> Result<(), String> {
        Ok(())
    }

    /// Every pack of tier `tier` closed: the tier is opened afresh (another writer's store,
    /// whose head changed).
    ///
    /// # Errors
    ///
    /// As [`Hot::pack_removed`], of the first pack that cannot be retired.
    #[cfg(not(blaze_check))]
    pub(crate) fn tier_closed(&mut self, tier: usize) -> Result<(), String> {
        let Ok(open) = self.open.as_ref() else { return Ok(()) };
        let packs: Vec<usize> = open
            .windows
            .files()
            .filter_map(|it| match it.id {
                FileId::Pack(id) if (id >> 16) as usize == tier => Some((id & 0xffff) as usize),
                _ => None,
            })
            .collect(); // alloc: a tier opened afresh, its packs listed once
        for pack in packs {
            self.pack_removed(tier, pack)?;
        }
        Ok(())
    }

    #[cfg(blaze_check)]
    pub(crate) fn tier_closed(&mut self, _tier: usize) -> Result<(), String> {
        Ok(())
    }

    /// The committed length the pool holds of pack `pack` of tier `tier`, and how many checks
    /// the loads core would check its reads by: a test's look.
    #[cfg(all(test, not(blaze_check)))]
    pub(crate) fn pack_open(&self, tier: usize, pack: usize) -> Option<u64> {
        let open = self.open.as_ref().ok()?;
        open.windows.get(pack_id(tier, pack)).map(|it| it.len)
    }
}
