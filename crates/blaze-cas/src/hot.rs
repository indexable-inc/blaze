//! The store's side of its buffer pool (docs/v2/plan/design/store-pool.md, lane 4b): the pool's
//! reservation, the table of windows and the loads core, and which of the store's files are
//! open in them.
//!
//! - **Packs.** A pack is opened in the pool with its blocks' checks as its tier opens
//!   ([`Hot::pack_opened`]), and retired with its file ([`Hot::pack_removed`]).
//! - **The commit writes through the pool.** A commit's new bytes of a pack go into the pack's
//!   window at the offsets they will have on disk ([`Hot::begin_append`], then
//!   [`crate::pool::Append::put`]), are written from there, and are the pack's loaded pages
//!   once the head names them ([`Hot::committed`]). The page the committed bytes end in is
//!   pinned first; when it is not in memory, the store asks its load as an effect of its own
//!   (a read into the page, tagged from `Config::load_tags`) and takes the completion
//!   ([`Hot::loaded`]), so the commit stays a sans-IO state machine.
//! - **A pinned read** of a pack's bytes whose miss the caller's [`Pump`] loads
//!   ([`Hot::pin_pack`]), and **the sweep** ([`Hot::evict`]). The store's readers do not read
//!   through the pool yet (it still maps what they read): these are what they will call.
//!
//! The pool is built under the model checker too (`cfg(blaze_check)`), with `std`'s atomics:
//! a model run schedules the store's tables and segments, not the pool's state words.

use std::{collections::BTreeMap, sync::Arc};

use blaze_core::{Cache, Completion, Effect, FileName, Tag};

use crate::{
    pool::{
        Append, Ended, FileId, Kernel, Load, LoadFailure, Loads, Opened, Pins, Pool, PoolError,
        Rank, ReadStep, Reading, Tail, WINDOW, WINDOWS, Windows,
        evict::{Sweep, Swept},
        loads::DEMAND_SLOTS,
    },
    pump::{Pump, PumpError},
};

/// One pack's append in a commit: what the commit holds until its head is written.
pub(crate) type PackAppend = Append<Kernel, Arc<Pool<Kernel>>>;

/// The load slot a pumped read loads in: a pumped store's reads are one at a time (the store
/// is its caller's alone while it pumps), so one slot serves them all. The rest are the
/// commit's, for the pages its appends pin.
const PUMPED_SLOT: u16 = 0;

/// A pack as the pool names it: its tier and its place in its tier's head.
fn pack_id(tier: usize, pack: usize) -> FileId {
    debug_assert!(tier < 1 << 8 && pack < crate::PACKS_MOST);
    FileId::Pack((tier << 16 | pack) as u32)
}

/// Why a pinned read of a pack's bytes failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum PinError {
    /// The store has no pool: its reservation was refused.
    NoPool(String),
    /// The pack is not open in the pool, or the bytes asked for run past what it commits.
    NotCommitted { tier: usize, pack: usize, offset: u64, len: u64, committed: Option<u64> },
    Pool(PoolError),
    Pump(PumpError),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::NoPool(why) => write!(f, "{why}"),
            PinError::NotCommitted { tier, pack, offset, len, committed: Some(committed) } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which commits {committed}, are \
                 not the store pool's to read",
                offset + len
            ),
            PinError::NotCommitted { tier, pack, offset, len, committed: None } => write!(
                f,
                "bytes {offset}..{} of pack {pack} of tier {tier}, which is not open in the \
                 store's pool",
                offset + len
            ),
            PinError::Pool(err) => write!(f, "{err}"),
            PinError::Pump(err) => write!(f, "{err}"),
        }
    }
}

/// `load` asked of the loads core and carried out by `pump`: how it ended.
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
            // Another's: the driver's other effects, or a read of a commit's own load (its
            // append's last page), which the pump keeps for the store.
            if loads.slot_of(completion.tag) != Some(load.slot) {
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

/// The pool, the windows of the files open in it, and the loads core that reads them.
struct Open {
    pool: Arc<Pool<Kernel>>,
    windows: Windows,
    loads: Loads,
    sweep: Sweep,
    /// The load slots no commit's append holds.
    free: Vec<u16>,
    /// Appends waiting for the load of their pack's last committed page: the load's slot, the
    /// pack, and the read that asked it.
    tails: Vec<(u16, usize, Reading)>,
    /// Commits made, and for each own pack written in the last two the commit that wrote it:
    /// its last page is the sweep's to pass over ([`Tail::Hot`]).
    commits: u64,
    written: BTreeMap<usize, u64>,
}

impl Open {
    /// The pinned read `reading` of the last committed page of own pack `pack` taken as far as
    /// it goes: `Ok(Some)` once the page is pinned and the append begun, `Ok(None)` while its
    /// load is in flight (asked into `effects`; [`Hot::loaded`] takes it on).
    fn advance(
        &mut self,
        pack: usize,
        mut reading: Reading,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<PackAppend>, String> {
        let mut spin = blaze_spin::SpinWait::new();
        // Bounded by the reading: it pins the page, asks a load or counts a wait each turn.
        loop {
            match reading.step(&self.pool) {
                ReadStep::Pinned(pins) => {
                    self.free.extend(reading.take_slot());
                    let window = *self
                        .windows
                        .get(pack_id(0, pack))
                        .expect("a pack whose last page was pinned is open");
                    // clone: a count of the pool, held by the append until its commit ends
                    return Ok(Some(Append::begin(self.pool.clone(), window, Some(pins))));
                }
                ReadStep::Failed(error) => {
                    self.free.extend(reading.take_slot());
                    // alloc: the error of a commit that fails
                    return Err(error.to_string());
                }
                ReadStep::NeedSlot => match self.free.pop() {
                    Some(slot) => reading.give_slot(slot),
                    None => {
                        reading.give_up(&*self.pool);
                        // alloc: the error of a commit that fails
                        return Err(format!(
                            "every load slot of the store's pool ({DEMAND_SLOTS}) is held by a \
                             pack this commit appends to"
                        ));
                    }
                },
                ReadStep::Ask(load) => {
                    // alloc: empty unless the load ends as it is asked
                    let mut ended = Vec::new();
                    self.loads.ask(&*self.pool, load, effects, &mut ended);
                    match ended.pop() {
                        // In no file the loads core knows: ended as it was asked, failed.
                        Some(Ended { outcome, .. }) => reading.loaded(&*self.pool, outcome),
                        None => {
                            // alloc: a commit's appends waiting on a load, a few
                            self.tails.push((load.slot, pack, reading));
                            return Ok(None);
                        }
                    }
                }
                ReadStep::Pending => unreachable!("a load in flight is kept in `tails`"),
                // The sweep is giving the page back, or a pumped read is loading it: the
                // reading bounds how often.
                ReadStep::Wait { .. } | ReadStep::Spin { .. } => {
                    if !spin.spin() {
                        spin = blaze_spin::SpinWait::new();
                    }
                }
            }
        }
    }
}

/// The store's hot tier.
pub(crate) struct Hot {
    open: Result<Open, String>,
}

impl Hot {
    /// The pool reserved, with nothing open in it, its loads asking with tags
    /// `load_tags..load_tags + 2^32`. A reservation the system refused is kept as why
    /// ([`Hot::refused`]): the store stops, naming it, and nothing is read another way.
    pub(crate) fn new(load_tags: u64) -> Hot {
        let open = Pool::new(WINDOWS, Kernel)
            .map(|pool| Open {
                // alloc: the pool's count, once per store
                pool: Arc::new(pool),
                windows: Windows::new(WINDOWS),
                // Around the page cache: the pool is the one copy of what it reads.
                loads: Loads::new(load_tags, Cache::Bypassed),
                sweep: Sweep::new(),
                // alloc: the commit's load slots, once per store
                free: (PUMPED_SLOT + 1..DEMAND_SLOTS as u16).rev().collect(),
                // alloc: empty, allocates nothing
                tails: Vec::new(),
                commits: 0,
                written: BTreeMap::new(),
            })
            // alloc: the error that stops the store
            .map_err(|err| err.to_string());
        Hot { open }
    }

    /// Why there is no pool, when its reservation was refused.
    pub(crate) fn refused(&self) -> Option<&str> {
        self.open.as_ref().err().map(String::as_str)
    }

    /// Whether `tag` is one its loads ask with.
    pub(crate) fn owns(&self, tag: Tag) -> bool {
        self.open.as_ref().is_ok_and(|open| open.loads.owns(tag))
    }

    /// Pack `pack` of tier `tier`, the file `file`, commits `committed` bytes whose blocks'
    /// checks are `checks`: opened in the pool, as its tier opens.
    ///
    /// # Errors
    ///
    /// The pool has no windows left for it: why, for the store to stop on, naming the file.
    pub(crate) fn pack_opened(
        &mut self,
        tier: usize,
        pack: usize,
        file: &FileName,
        committed: u64,
        checks: &[u32],
    ) -> Result<(), String> {
        // clone: the error that stops the store
        let open = self.open.as_mut().map_err(|why| why.clone())?;
        let window = open
            .windows
            .open(&*open.pool, pack_id(tier, pack), committed, Rank::Plain)
            // alloc: the error that stops the store
            .map_err(|err| err.to_string())?;
        // clone: the loads core keeps the name for its reads, and the checks to check them by
        open.loads.open(Opened { window, name: file.clone(), checks: checks.to_vec() });
        Ok(())
    }

    /// A commit's append to own pack `pack`, the file `file`, begun: the pack will commit
    /// `to` bytes. A pack not yet in the pool (made by this commit) is opened empty, with
    /// windows for `to`. `Ok(None)` when the page its committed bytes end in must be loaded
    /// first: the read is asked into `effects`, and [`Hot::loaded`] gives the append once it
    /// completes.
    ///
    /// # Errors
    ///
    /// Why the append cannot begin: no pool, no windows, a pack that would outgrow the
    /// windows it has, or its last page refused (a block of it failed its check).
    pub(crate) fn begin_append(
        &mut self,
        pack: usize,
        file: &FileName,
        to: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<PackAppend>, String> {
        // clone: the error of a commit that fails
        let open = self.open.as_mut().map_err(|why| why.clone())?;
        let id = pack_id(0, pack);
        let window = match open.windows.get(id).copied() {
            Some(window) => window,
            None => {
                let window = open
                    .windows
                    .open_room(&*open.pool, id, 0, to, Rank::Plain)
                    // alloc: the error of a commit that fails
                    .map_err(|err| err.to_string())?;
                // clone: the loads core keeps the name for its reads
                // alloc: empty, allocates nothing
                open.loads.open(Opened { window, name: file.clone(), checks: Vec::new() });
                window
            }
        };
        if to > u64::from(window.count) * WINDOW {
            // alloc: the error of a commit that fails
            return Err(format!(
                "it would commit {to} bytes, past the {} windows of the store's pool it was \
                 opened with at {} bytes",
                window.count, window.len
            ));
        }
        match PackAppend::tail_of(&window) {
            // clone: a count of the pool, held by the append until its commit ends
            None => Ok(Some(Append::begin(open.pool.clone(), window, None))),
            Some((at, len)) => open.advance(pack, Reading::new(at, len), effects),
        }
    }

    /// A completion of one of its loads' reads: each append whose pack's last page it was the
    /// load of, begun, or why it cannot be.
    pub(crate) fn loaded(
        &mut self,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<(usize, Result<PackAppend, String>)> {
        // alloc: empty unless a load ended; then the appends it frees, a few
        let mut out = Vec::new();
        let Ok(open) = self.open.as_mut() else { return out };
        // alloc: empty unless a load ended
        let mut ended = Vec::new();
        open.loads.complete(&*open.pool, completion, &mut ended);
        for Ended { slot, outcome } in ended {
            let Some(at) = open.tails.iter().position(|it| it.0 == slot) else { continue };
            let (_, pack, mut reading) = open.tails.swap_remove(at);
            reading.loaded(&*open.pool, outcome);
            match open.advance(pack, reading, effects) {
                Ok(Some(append)) => out.push((pack, Ok(append))),
                // Evicted again before it was pinned: its load is asked again.
                Ok(None) => {}
                Err(why) => out.push((pack, Err(why))),
            }
        }
        out
    }

    /// The head names what `append` wrote of own pack `pack`: its pages are the pack's loaded
    /// pages, `checks` (those of its new blocks) what their reloads are checked by.
    pub(crate) fn committed(&mut self, pack: usize, append: PackAppend, checks: &[u32]) {
        let open = self.open.as_mut().expect("an append is of a store with a pool");
        let window = append.commit(&mut open.windows);
        // alloc: the loads core keeps the new blocks' checks beside the rest
        open.loads.grow(window, checks.to_vec());
        open.windows.set_tail(pack_id(0, pack), Tail::Hot);
        open.written.insert(pack, open.commits);
    }

    /// A commit ended: the last page of a pack not written in the last two is the sweep's
    /// again.
    pub(crate) fn commit_ended(&mut self) {
        let Ok(open) = self.open.as_mut() else { return };
        open.commits += 1;
        let (commits, windows) = (open.commits, &mut open.windows);
        open.written.retain(|&pack, &mut at| {
            let hot = at + 2 >= commits;
            if !hot {
                windows.set_tail(pack_id(0, pack), Tail::Cold);
            }
            hot
        });
    }

    /// Pack `pack` of tier `tier` is removed: its pages given back and its windows freed. One
    /// never opened is nothing to do.
    ///
    /// # Errors
    ///
    /// A page of it is pinned or loading: why. Its windows stay taken.
    pub(crate) fn pack_removed(&mut self, tier: usize, pack: usize) -> Result<(), String> {
        let Ok(open) = self.open.as_mut() else { return Ok(()) };
        let id = pack_id(tier, pack);
        let Some(window) = open.windows.get(id).copied() else { return Ok(()) };
        // alloc: a problem reported
        open.windows.retire(&*open.pool, id).map_err(|err| err.to_string())?;
        open.loads.close(window.first);
        if tier == 0 {
            open.written.remove(&pack);
        }
        Ok(())
    }

    /// Every pack of tier `tier` closed: the tier is opened afresh (another writer's store,
    /// whose head changed).
    ///
    /// # Errors
    ///
    /// As [`Hot::pack_removed`], of the first pack that cannot be retired.
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

    /// The `len` (at least one) bytes at `offset` of pack `pack` of tier `tier` pinned: in
    /// memory already, or loaded now by `pump`, each block checked as it is. The bytes stay as
    /// they are while the guard lives.
    ///
    /// A pump that stalls leaves its load in flight: the pages it claimed stay `Loading`, and a
    /// later read of them ends starved, naming the page. The store's driver has stopped; so has
    /// the store.
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
        let (pool, loads): (&Pool<Kernel>, &mut Loads) = (&*open.pool, &mut open.loads);
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
                ReadStep::Pending => {
                    unreachable!("a pumped load has ended by the time it is looked at")
                }
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
    pub(crate) fn evict(&mut self, bytes: u64) -> Option<Swept> {
        let open = self.open.as_mut().ok()?;
        Some(open.sweep.evict(&*open.pool, &open.windows, bytes))
    }

    /// The committed length the pool holds of pack `pack` of tier `tier`: a test's look.
    #[cfg(test)]
    pub(crate) fn pack_open(&self, tier: usize, pack: usize) -> Option<u64> {
        let open = self.open.as_ref().ok()?;
        open.windows.get(pack_id(tier, pack)).map(|it| it.len)
    }
}
