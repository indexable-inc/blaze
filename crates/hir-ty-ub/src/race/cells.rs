//! The clocks of a thread and of a byte of memory, and the rules that find a race at one access,
//! ported from Miri's `src/tools/miri/src/concurrency/data_race.rs` (`ThreadClockSet`,
//! `MemoryCellClocks`, `AtomicMemoryCellClocks`).

use crate::ids::SiteId;

use super::{
    error::Action,
    vclock::{NaReadType, VClock, VTimestamp, VectorIdx, is_dummy},
};

/// A thread's clocks (Miri: `ThreadClockSet`).
#[derive(Clone, Default, Debug)]
pub(super) struct ThreadClockSet {
    /// What happens before this thread.
    pub(super) clock: VClock,
    /// What will happen before this thread once it runs an acquire fence.
    pub(super) fence_acquire: VClock,
    /// What this thread released at its last release fence.
    pub(super) fence_release: VClock,
    /// The last SC write of each thread as of this thread's last SC fence; it only limits which
    /// old writes a weak load may see.
    pub(super) write_seqcst: VClock,
    /// The last SC fence of each thread as of this thread's last SC read; likewise.
    pub(super) read_seqcst: VClock,
}

impl ThreadClockSet {
    #[inline]
    pub(super) fn apply_release_fence(&mut self) {
        self.fence_release.clone_from(&self.clock);
    }

    #[inline]
    pub(super) fn apply_acquire_fence(&mut self) {
        self.clock.join(&self.fence_acquire);
    }

    #[inline]
    pub(super) fn increment_clock(&mut self, index: VectorIdx, site: SiteId) {
        self.clock.increment_index(index, site);
    }

    pub(super) fn join_with(&mut self, other: &ThreadClockSet) {
        self.clock.join(&other.clock);
    }
}

/// An access raced: the cell says with what, once [`super::Races`] reports it.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct DataRace;

/// The clocks of a cell that was accessed atomically; kept apart because most memory never is
/// (Miri: `AtomicMemoryCellClocks`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct AtomicMemoryCellClocks {
    /// The last atomic read of each thread, against non-atomic writes.
    pub(super) read_vector: VClock,
    /// The last atomic write of each thread, against non-atomic reads and writes.
    pub(super) write_vector: VClock,
    /// What an acquire load of this cell acquires. Under weak memory this is the clock of the
    /// latest store, used for release sequences (an RMW joins it into the next one).
    pub(super) sync_vector: VClock,
    /// The size of the atomic accesses here; `None` once reads of different sizes were seen
    /// (fine while all of them are reads). Atomics are aligned to their size, so the size alone
    /// finds an imperfectly overlapping access.
    pub(super) size: Option<u64>,
}

impl AtomicMemoryCellClocks {
    fn new(size: u64) -> Self {
        AtomicMemoryCellClocks {
            read_vector: VClock::default(),
            write_vector: VClock::default(),
            sync_vector: VClock::default(),
            size: Some(size),
        }
    }
}

/// What a non-atomic write was (Miri: `NaWriteType`): allocation and deallocation are writes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum NaWriteType {
    Allocate,
    Write,
    Retag,
    Deallocate,
}

/// An access, as the race detector tells them apart (Miri: `AccessType`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(super) enum AccessType {
    NaRead(NaReadType),
    NaWrite(NaWriteType),
    AtomicLoad,
    AtomicStore,
    AtomicRmw,
}

impl AccessType {
    pub(super) fn is_atomic(self) -> bool {
        matches!(self, AccessType::AtomicLoad | AccessType::AtomicStore | AccessType::AtomicRmw)
    }

    pub(super) fn is_read(self) -> bool {
        matches!(self, AccessType::AtomicLoad | AccessType::NaRead(_))
    }

    pub(super) fn is_retag(self) -> bool {
        matches!(
            self,
            AccessType::NaRead(NaReadType::Retag) | AccessType::NaWrite(NaWriteType::Retag)
        )
    }

    pub(super) fn action(self) -> Action {
        match self {
            AccessType::NaRead(NaReadType::Read) => Action::Read,
            AccessType::NaRead(NaReadType::Retag) => Action::RetagRead,
            AccessType::NaWrite(NaWriteType::Allocate) => Action::Allocate,
            AccessType::NaWrite(NaWriteType::Write) => Action::Write,
            AccessType::NaWrite(NaWriteType::Retag) => Action::RetagWrite,
            AccessType::NaWrite(NaWriteType::Deallocate) => Action::Deallocate,
            AccessType::AtomicLoad => Action::AtomicLoad,
            AccessType::AtomicStore => Action::AtomicStore,
            AccessType::AtomicRmw => Action::AtomicRmw,
        }
    }
}

/// The clocks of a run of bytes (Miri: `MemoryCellClocks`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct MemoryCellClocks {
    /// The thread and time of the last non-atomic write; a single timestamp suffices, since
    /// nothing synchronizes a non-atomic write.
    pub(super) write: (VectorIdx, VTimestamp),
    pub(super) write_type: NaWriteType,
    /// The non-atomic reads since the last non-atomic write, joined.
    pub(super) read: VClock,
    /// `None` for memory never accessed atomically since its last non-atomic write. Boxed as
    /// Miri does: one per atomic location, never per access.
    pub(super) atomic_ops: Option<Box<AtomicMemoryCellClocks>>,
}

impl MemoryCellClocks {
    /// Memory allocated by `alloc_index` at `alloc`.
    pub(super) fn new(alloc: VTimestamp, alloc_index: VectorIdx) -> Self {
        MemoryCellClocks {
            read: VClock::default(),
            write: (alloc_index, alloc),
            write_type: NaWriteType::Allocate,
            atomic_ops: None,
        }
    }

    /// `self.write() <= other`, without making the clock.
    #[inline]
    fn write_was_before(&self, other: &VClock) -> bool {
        self.write.1 <= other[self.write.0]
    }

    #[inline]
    pub(super) fn write(&self) -> VClock {
        VClock::new_with_index(self.write.0, self.write.1)
    }

    #[inline]
    pub(super) fn atomic(&self) -> Option<&AtomicMemoryCellClocks> {
        self.atomic_ops.as_deref()
    }

    #[inline]
    fn atomic_mut_unwrap(&mut self) -> &mut AtomicMemoryCellClocks {
        self.atomic_ops.as_deref_mut().unwrap()
    }

    /// The atomic clocks, made if missing, after the mixed-size rule: a size change is fine
    /// when every earlier atomic access happens before this one, and a read of another size
    /// when every earlier write does (Miri: `atomic_access`).
    fn atomic_access(
        &mut self,
        thread_clocks: &ThreadClockSet,
        size: u64,
        write: bool,
    ) -> Result<&mut AtomicMemoryCellClocks, DataRace> {
        match self.atomic_ops {
            Some(ref mut atomic) => {
                if atomic.size == Some(size) {
                    Ok(atomic)
                } else if atomic.read_vector <= thread_clocks.clock
                    && atomic.write_vector <= thread_clocks.clock
                {
                    atomic.size = Some(size);
                    Ok(atomic)
                } else if !write && atomic.write_vector <= thread_clocks.clock {
                    atomic.size = None;
                    Ok(atomic)
                } else {
                    Err(DataRace)
                }
            }
            None => Ok(self.atomic_ops.insert(Box::new(AtomicMemoryCellClocks::new(size)))),
        }
    }

    /// An acquire load (Miri: `load_acquire`). `sync_clock` is the clock of the store a weak
    /// load read from; `None` means the latest store, this cell's own.
    pub(super) fn load_acquire(
        &mut self,
        thread_clocks: &mut ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
        sync_clock: Option<&VClock>,
    ) -> Result<(), DataRace> {
        self.atomic_read_detect(thread_clocks, index, access_size)?;
        if let Some(sync_clock) = sync_clock.or_else(|| self.atomic().map(|a| &a.sync_vector)) {
            thread_clocks.clock.join(sync_clock);
        }
        Ok(())
    }

    /// A relaxed load: what it would acquire waits for an acquire fence (Miri: `load_relaxed`).
    pub(super) fn load_relaxed(
        &mut self,
        thread_clocks: &mut ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
        sync_clock: Option<&VClock>,
    ) -> Result<(), DataRace> {
        self.atomic_read_detect(thread_clocks, index, access_size)?;
        if let Some(sync_clock) = sync_clock.or_else(|| self.atomic().map(|a| &a.sync_vector)) {
            thread_clocks.fence_acquire.join(sync_clock);
        }
        Ok(())
    }

    /// A release store (Miri: `store_release`).
    pub(super) fn store_release(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        self.atomic_write_detect(thread_clocks, index, access_size)?;
        let atomic = self.atomic_mut_unwrap();
        atomic.sync_vector.clone_from(&thread_clocks.clock);
        Ok(())
    }

    /// A relaxed store. Since C++20 every relaxed store ends the release sequences before it,
    /// the storing thread's own too, so the sync clock is overwritten with what the thread's
    /// last release fence released (Miri: `store_relaxed`).
    pub(super) fn store_relaxed(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        self.atomic_write_detect(thread_clocks, index, access_size)?;
        let atomic = self.atomic_mut_unwrap();
        atomic.sync_vector.clone_from(&thread_clocks.fence_release);
        Ok(())
    }

    /// The store of a release RMW: joining keeps the release sequence going (Miri:
    /// `rmw_release`).
    pub(super) fn rmw_release(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        self.atomic_write_detect(thread_clocks, index, access_size)?;
        let atomic = self.atomic_mut_unwrap();
        atomic.sync_vector.join(&thread_clocks.clock);
        Ok(())
    }

    /// The store of a relaxed RMW (Miri: `rmw_relaxed`).
    pub(super) fn rmw_relaxed(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        self.atomic_write_detect(thread_clocks, index, access_size)?;
        let atomic = self.atomic_mut_unwrap();
        atomic.sync_vector.join(&thread_clocks.fence_release);
        Ok(())
    }

    /// An atomic read races with a non-atomic write that doesn't happen before it (Miri:
    /// `atomic_read_detect`).
    fn atomic_read_detect(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        let atomic = self.atomic_access(thread_clocks, access_size, false)?;
        atomic.read_vector.set_at_index(&thread_clocks.clock, index);
        if self.write_was_before(&thread_clocks.clock) { Ok(()) } else { Err(DataRace) }
    }

    /// An atomic write races with a non-atomic read or write that doesn't happen before it
    /// (Miri: `atomic_write_detect`).
    fn atomic_write_detect(
        &mut self,
        thread_clocks: &ThreadClockSet,
        index: VectorIdx,
        access_size: u64,
    ) -> Result<(), DataRace> {
        let atomic = self.atomic_access(thread_clocks, access_size, true)?;
        atomic.write_vector.set_at_index(&thread_clocks.clock, index);
        if self.write_was_before(&thread_clocks.clock) && self.read <= thread_clocks.clock {
            Ok(())
        } else {
            Err(DataRace)
        }
    }

    /// A non-atomic read races with any write that doesn't happen before it (Miri:
    /// `non_atomic_read_detect`).
    pub(super) fn non_atomic_read_detect(
        &mut self,
        thread_clocks: &mut ThreadClockSet,
        index: VectorIdx,
        read_type: NaReadType,
        site: SiteId,
    ) -> Result<(), DataRace> {
        if !is_dummy(site) {
            thread_clocks.clock.index_mut(index).site = site;
        }
        thread_clocks.clock.index_mut(index).set_read_type(read_type);
        if !self.write_was_before(&thread_clocks.clock) {
            return Err(DataRace);
        }
        if !self.atomic().is_none_or(|atomic| atomic.write_vector <= thread_clocks.clock) {
            return Err(DataRace);
        }
        self.read.set_at_index(&thread_clocks.clock, index);
        Ok(())
    }

    /// A non-atomic write races with any access that doesn't happen before it, and makes the
    /// cell non-atomic again (Miri: `non_atomic_write_detect`).
    pub(super) fn non_atomic_write_detect(
        &mut self,
        thread_clocks: &mut ThreadClockSet,
        index: VectorIdx,
        write_type: NaWriteType,
        site: SiteId,
    ) -> Result<(), DataRace> {
        if !is_dummy(site) {
            thread_clocks.clock.index_mut(index).site = site;
        }
        if !(self.write_was_before(&thread_clocks.clock) && self.read <= thread_clocks.clock) {
            return Err(DataRace);
        }
        if !self.atomic().is_none_or(|atomic| {
            atomic.write_vector <= thread_clocks.clock && atomic.read_vector <= thread_clocks.clock
        }) {
            return Err(DataRace);
        }
        self.write = (index, thread_clocks.clock[index]);
        self.write_type = write_type;
        self.read.set_zero_vector();
        self.atomic_ops = None;
        Ok(())
    }
}
