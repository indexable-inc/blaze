//! The store's side of its buffer pool (docs/v2/plan/design/store-pool.md, lane 4b): the pool's
//! reservation, the table of windows and the loads core, and which of the store's files are
//! open in them.
//!
//! A pack is opened in the pool with its blocks' checks as its tier opens, grows as a commit
//! makes more of it durable, and is retired with its file. The store's readers do not read
//! through the pool yet (it still maps what they read); this is the state they move onto, and
//! what they will call: [`Hot::pin_pack`], a pinned read of a pack's bytes whose miss is loaded
//! by the caller's [`Pump`], and [`Hot::evict`], the sweep.
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
use crate::{
    pool::{
        Ended, FileId, Kernel, Load, LoadFailure, Loads, Opened, PAGE, Pins, Pool, PoolError, Rank,
        ReadStep, Reading, WINDOW, WINDOWS, Windows,
        evict::{Sweep, Swept},
    },
    pump::{Pump, PumpError},
};

/// The load slot a pumped read loads in: a pumped store's reads are one at a time (the store
/// is its caller's alone while it pumps), so one slot serves them all.
#[cfg(not(blaze_check))]
const PUMPED_SLOT: u16 = 0;

/// Why a pinned read of a pack's bytes failed.
#[cfg(not(blaze_check))]
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum PinError {
    /// The store has no pool: its reservation was refused.
    NoPool(String),
    /// The pack is not open in the pool, or the bytes asked for run past what it commits.
    NotCommitted { tier: usize, pack: usize, offset: u64, len: u64, committed: Option<u64> },
    Pool(PoolError),
    Pump(PumpError),
}

#[cfg(not(blaze_check))]
impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::NoPool(why) => write!(f, "{why}"),
            PinError::NotCommitted { tier, pack, offset, len, committed: Some(committed) } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which commits {committed}, are                  not the store pool's to read",
                offset + len
            ),
            PinError::NotCommitted { tier, pack, offset, len, committed: None } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which is not open in the                  store's pool",
                offset + len
            ),
            PinError::Pool(err) => write!(f, "{err}"),
            PinError::Pump(err) => write!(f, "{err}"),
        }
    }
}

/// `load` asked of the loads core and carried out by `pump`: how it ended.
#[cfg(not(blaze_check))]
fn pumped(
    pool: &Pool<Kernel>,
    loads: &mut Loads,
    pump: &mut dyn Pump,
    load: Load,
) -> Result<Result<(), LoadFailure>, PumpError> {
    // alloc: a pumped miss, a test's or a tool's, never the daemon's
    let (mut effects, mut ended) = (Vec::new(), Vec::new());
    loads.ask(pool, load, &mut effects, &mut ended);
    // A load of no file the core knows ended as it was asked, with no read.
    if ended.is_empty() {
        pump.pump(&mut effects, &mut |completion| {
            if !loads.owns(completion.tag) {
                return Some(completion);
            }
            loads.complete(pool, completion, &mut ended);
            None
        })?;
    }
    let Ended { outcome, .. } =
        ended.pop().expect("a pump that returns Ok has completed every read of the load");
    Ok(outcome)
}

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
    sweep: Sweep,
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
                sweep: Sweep::new(),
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
        let Open { pool, windows, loads, .. } = self.open.as_mut().map_err(|why| why.clone())?;
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
        let Ok(Open { pool, windows, loads, .. }) = self.open.as_mut() else { return Ok(()) };
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

    /// The `len` (at least one) bytes at `offset` of pack `pack` of tier `tier` pinned: in
    /// memory already, or loaded now by `pump`, each block checked as it is. The bytes stay as
    /// they are while the guard lives.
    ///
    /// A pump that stalls leaves its load in flight: the pages it claimed stay `Loading`, and a
    /// later read of them ends starved, naming the page. The store's driver has stopped; so has
    /// the store.
    #[cfg(not(blaze_check))]
    pub(crate) fn pin_pack(
        &mut self,
        pump: &mut dyn Pump,
        tier: usize,
        pack: usize,
        offset: u64,
        len: u64,
    ) -> Result<Pins<Kernel, &Pool<Kernel>>, PinError> {
        // clone: the error of a read
        let open = self.open.as_mut().map_err(|why| PinError::NoPool(why.clone()))?;
        let (pool, loads): (&Pool<Kernel>, &mut Loads) = (&open.pool, &mut open.loads);
        let committed = open.windows.get(pack_id(tier, pack)).map(|it| it.len);
        let window = open
            .windows
            .get(pack_id(tier, pack))
            .filter(|it| len >= 1 && offset.checked_add(len).is_some_and(|end| end <= it.len))
            .ok_or(PinError::NotCommitted { tier, pack, offset, len, committed })?;
        let mut reading = Reading::new(window.at(offset), len);
        let mut spin = blaze_spin::SpinWait::new();
        // Bounded by the reading: it pins a page or asks a load each turn, and counts its
        // waits to `pool::WAITS_MOST`.
        loop {
            match reading.step(&pool) {
                ReadStep::Pinned(pins) => {
                    reading.take_slot();
                    return Ok(pins);
                }
                ReadStep::Failed(error) => {
                    reading.take_slot();
                    return Err(PinError::Pool(error));
                }
                ReadStep::NeedSlot => reading.give_slot(PUMPED_SLOT),
                ReadStep::Ask(load) => match pumped(pool, loads, pump, load) {
                    Ok(outcome) => reading.loaded(pool, outcome),
                    Err(stalled) => {
                        // Its load is in flight for good: its claim and its slot go with it.
                        std::mem::forget(reading);
                        return Err(PinError::Pump(stalled));
                    }
                },
                ReadStep::Pending => unreachable!("a pumped load has ended by the time it is looked at"),
                // A page being given back, or loading in a pump that stalled: the reading
                // bounds how often.
                ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    if !spin.spin() {
                        spin = blaze_spin::SpinWait::new();
                    }
                }
            }
        }
    }

    /// At least `bytes` of the pool's pages given back, if the sweep finds them: the pool's
    /// `evict(bytes) -> freed`.
    #[cfg(not(blaze_check))]
    pub(crate) fn evict(&mut self, bytes: u64) -> Option<Swept> {
        let open = self.open.as_mut().ok()?;
        Some(open.sweep.evict(&open.pool, &open.windows, bytes))
    }

    /// The committed length the pool holds of pack `pack` of tier `tier`: a test's look.
    #[cfg(all(test, not(blaze_check)))]
    pub(crate) fn pack_open(&self, tier: usize, pack: usize) -> Option<u64> {
        let open = self.open.as_ref().ok()?;
        open.windows.get(pack_id(tier, pack)).map(|it| it.len)
    }
}
