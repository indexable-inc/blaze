//! Generational driver-branded handles (`#[blaze::handle]`), typestate handle tokens
//! (`File<'io, Unflushed>` -> `File<'io, Flushed>`, `Dir<'io, Mode>`, `Conn<'io>`, `Child<'io>`),
//! and the sans-IO [`HandleTable`] with deterministic slot allocation, `u32::MAX` retirement,
//! task-ownership reclamation, and loud leak detection
//! (`docs/v2/plan/design/sans-io-machines.md`, section 2).
//!
//! Layout guarantees:
//! - [`Handle<'io, K>`] is **8 bytes** (`slot: u32`, `gen: NonZeroU32`, 0-byte `Brand<'io>`).
//! - `Option<Handle<'io, K>>` is **8 bytes** via the `NonZeroU32` niche.
//! - Generation starts at `1` and increments on every release; when a slot reaches `u32::MAX`, the
//!   slot is permanently retired so a stale handle can never alias a later open.
//! - Invariant `'io` branding (`blaze_own::Brand<'io>`) statically prevents mixing handles across
//!   two driver or `Sim` instances.

use core::{
    fmt,
    marker::PhantomData,
    mem::size_of,
    num::NonZeroU32,
};

use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Brand, Vec};

// ---------------------------------------------------------------------------
// Handle kinds & 8-byte generational branded `Handle<'io, K>`
// ---------------------------------------------------------------------------

/// Handle kind marker: an open file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileKind;
/// Handle kind marker: an open directory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DirKind;
/// Handle kind marker: a stream network connection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnKind;
/// Handle kind marker: a bound listener.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ListenerKind;
/// Handle kind marker: a datagram socket.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SocketKind;
/// Handle kind marker: a spawned child process.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChildKind;
/// Handle kind marker: a filesystem watch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WatchKind;
/// Handle kind marker: a memory mapping.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MapKind;
/// Handle kind marker: an advisory file lock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LockKind;

/// Inert 8-byte generational handle branded to driver scope `'io`.
///
/// Allowed inside `#![blaze::sans_io]` state structs (`#[blaze::handle]`); carries zero executing
/// authority and cannot be serialized to cache records.
#[blaze::handle]
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Handle<'io, K> {
    slot: u32,
    generation: NonZeroU32,
    _brand: Brand<'io>,
    _kind: PhantomData<K>,
}

impl<'io, K> Copy for Handle<'io, K> {}
impl<'io, K> Clone for Handle<'io, K> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

const _: () = assert!(size_of::<Handle<'_, FileKind>>() == 8);
const _: () = assert!(size_of::<Option<Handle<'_, FileKind>>>() == 8);

impl<'io, K> Handle<'io, K> {
    #[inline]
    pub(crate) const fn new(slot: u32, generation: NonZeroU32, brand: Brand<'io>) -> Self {
        Self { slot, generation, _brand: brand, _kind: PhantomData }
    }

    #[inline]
    pub const fn slot(self) -> u32 {
        self.slot
    }

    #[inline]
    pub const fn generation(self) -> NonZeroU32 {
        self.generation
    }

    #[inline]
    pub const fn brand(self) -> Brand<'io> {
        self._brand
    }
}

pub type FileId<'io> = Handle<'io, FileKind>;
pub type DirId<'io> = Handle<'io, DirKind>;
pub type ConnId<'io> = Handle<'io, ConnKind>;
pub type ListenerId<'io> = Handle<'io, ListenerKind>;
pub type SocketId<'io> = Handle<'io, SocketKind>;
pub type ChildId<'io> = Handle<'io, ChildKind>;
pub type WatchId<'io> = Handle<'io, WatchKind>;
pub type MapId<'io> = Handle<'io, MapKind>;
pub type LockId<'io> = Handle<'io, LockKind>;

// ---------------------------------------------------------------------------
// Typestate handle tokens (`#[blaze::handle]`, `#[blaze::linear]` where closable)
// ---------------------------------------------------------------------------

/// File/directory access mode: read-only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadOnly;
/// Writable file mode whose recent writes have NOT yet been flushed durably.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unflushed;
/// Writable file mode whose bytes are proven flushed (`Flush` completed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flushed;
/// Directory mode allowing mutations (open for write, rename, sync_dir).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadWrite;

/// Linear typestate handle to an open file in driver scope `'io`.
///
/// Has no `Drop`: must be consumed via `file.close()` (emitting a [`crate::request::Close`]
/// request) or handed on to the next state. Writable files start in [`Unflushed`] and transition
/// to [`Flushed`] only through `file.flush()`.
#[blaze::handle]
#[blaze::linear]
#[derive(Debug)]
pub struct File<'io, Mode> {
    id: FileId<'io>,
    _mode: PhantomData<Mode>,
}

const _: () = assert!(size_of::<File<'_, ReadOnly>>() == 8);
const _: () = assert!(size_of::<Option<File<'_, Unflushed>>>() == 8);

impl<'io, Mode> File<'io, Mode> {
    #[inline]
    pub(crate) const fn from_id(id: FileId<'io>) -> Self {
        Self { id, _mode: PhantomData }
    }

    #[inline]
    pub const fn id(&self) -> FileId<'io> {
        self.id
    }

    /// Consumes this linear file handle into its raw [`FileId`] (driver/request boundary only).
    #[blaze::consumes]
    #[inline]
    pub(crate) const fn into_id(self) -> FileId<'io> {
        self.id
    }
}

/// Inert handle to a scoped directory in driver scope `'io`.
#[blaze::handle]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dir<'io, Mode = ReadWrite> {
    id: DirId<'io>,
    _mode: PhantomData<Mode>,
}

impl<'io, Mode> Dir<'io, Mode> {
    #[inline]
    pub(crate) const fn from_id(id: DirId<'io>) -> Self {
        Self { id, _mode: PhantomData }
    }

    #[inline]
    pub const fn id(&self) -> DirId<'io> {
        self.id
    }

    /// Monotonically narrows any directory handle to read-only (rule 73).
    #[inline]
    pub const fn narrow_read_only(self) -> Dir<'io, ReadOnly> {
        Dir { id: self.id, _mode: PhantomData }
    }
}

/// Linear handle to an open stream connection in driver scope `'io`.
#[blaze::handle]
#[blaze::linear]
#[derive(Debug)]
pub struct Conn<'io> {
    id: ConnId<'io>,
}

impl<'io> Conn<'io> {
    #[inline]
    pub(crate) const fn from_id(id: ConnId<'io>) -> Self {
        Self { id }
    }

    #[inline]
    pub const fn id(&self) -> ConnId<'io> {
        self.id
    }

    #[blaze::consumes]
    #[inline]
    pub(crate) const fn into_id(self) -> ConnId<'io> {
        self.id
    }
}

/// Linear handle to a spawned child process in driver scope `'io`.
#[blaze::handle]
#[blaze::linear]
#[derive(Debug)]
pub struct Child<'io> {
    id: ChildId<'io>,
}

impl<'io> Child<'io> {
    #[inline]
    pub(crate) const fn from_id(id: ChildId<'io>) -> Self {
        Self { id }
    }

    #[inline]
    pub const fn id(&self) -> ChildId<'io> {
        self.id
    }

    #[blaze::consumes]
    #[inline]
    pub(crate) const fn into_id(self) -> ChildId<'io> {
        self.id
    }
}

// ---------------------------------------------------------------------------
// Sans-IO generational `HandleTable` with task ownership & leak detection
// ---------------------------------------------------------------------------

/// Owning task identifier stored beside each live handle for cancellation reclamation and
/// normal-exit leak detection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TaskId(pub u32);

/// How a task terminated when asking [`HandleTable::reclaim_task`] to sweep its handles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitKind {
    /// Task ended via `Done` or `Fail`: any still-open handle is a leak bug.
    Normal,
    /// Task was externally cancelled (`Stopped`): reclaiming open handles is the intended backstop.
    Cancelled,
}

/// Typed refusal for stale, invalid, or wrong-owner handle operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// Slot index is outside the table's allocated range.
    InvalidSlot { slot: u32 },
    /// Handle refers to a closed, retired, or reused slot (generation mismatch).
    Stale { slot: u32, expected: Option<NonZeroU32>, found: NonZeroU32 },
    /// Handle belongs to a different task than `caller`.
    WrongOwner { slot: u32, owner: TaskId, caller: TaskId },
    /// Table reached its explicit slot bound (`max_slots`, rule 18 `bounded-progress`).
    TableFull { max_slots: u32 },
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HandleError::InvalidSlot { slot } => write!(f, "invalid handle slot {slot}"),
            HandleError::Stale { slot, expected, found } => {
                write!(f, "stale handle at slot {slot}: found gen {found}, table has {expected:?}")
            }
            HandleError::WrongOwner { slot, owner, caller } => {
                write!(f, "handle at slot {slot} belongs to {owner:?}, not {caller:?}")
            }
            HandleError::TableFull { max_slots } => {
                write!(f, "handle table full at cap {max_slots}")
            }
        }
    }
}

/// Loud error returned when a task exits normally (`Done`/`Fail`) while still holding open
/// handles (never silently ignored in `Sim` or tests).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LeakedHandles {
    pub task: TaskId,
    pub count: u32,
}

impl fmt::Display for LeakedHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "task {:?} exited normally with {} unclosed handle(s)",
            self.task, self.count
        )
    }
}

/// Planted-bug switches for `HandleTable` DST controls (rule 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum HandleTablePlant {
    #[default]
    None,
    /// Planted bug: wrap generation `u32::MAX -> 1` instead of retiring the slot (allows a stale
    /// handle from gen 1 to alias a brand-new open!).
    WrapGenerationOnMax,
    /// Planted bug: track `(task, slot)` without generation check on reclaim, closing a slot that
    /// was already closed and reused by a sibling task.
    ReclaimStaleSlotWithoutOwnerCheck,
}

impl Plants for HandleTablePlant {}

/// Options for [`HandleTable::new`] (`#[blaze::args]`, rule 83 `options-struct`).
#[blaze::args]
pub struct HandleTableOpts<'io, 'a> {
    pub brand: Brand<'io>,
    pub max_slots: u32,
    pub alloc: Alloc<'a>,
}

enum SlotState<V> {
    /// Occupied by `owner` at generation `gen`.
    Live { owner: TaskId, value: V },
    /// Free and eligible for deterministic reuse at `next_gen`.
    Vacant,
    /// Generation reached `u32::MAX`: permanently retired so stale handles can never alias.
    Retired,
}

struct Slot<V> {
    generation: NonZeroU32,
    state: SlotState<V>,
}

/// Sans-IO generational handle table branded to driver `'io` and backed by an explicit allocator.
pub struct HandleTable<'io, K, V, P: Planted<HandleTablePlant> = NoPlant> {
    brand: Brand<'io>,
    max_slots: u32,
    slots: Vec<Slot<V>, Alloc<'io>>,
    free_list: Vec<u32, Alloc<'io>>,
    live_count: u32,
    retired_count: u32,
    /// Records every `(task, slot)` ever inserted for the `ReclaimStaleSlotWithoutOwnerCheck`
    /// planted-bug control; untouched in production (`NoPlant`).
    planted_task_slots: Vec<(TaskId, u32), Alloc<'io>>,
    plant: P,
    _kind: PhantomData<K>,
}

impl<'io, K, V> HandleTable<'io, K, V, NoPlant> {
    /// Creates a bounded, empty handle table.
    pub fn new(opts: HandleTableOpts<'io, 'io>) -> Self {
        Self {
            brand: opts.brand,
            max_slots: opts.max_slots,
            slots: Vec::new_in(opts.alloc),
            free_list: Vec::new_in(opts.alloc),
            live_count: 0,
            retired_count: 0,
            planted_task_slots: Vec::new_in(opts.alloc),
            plant: NoPlant,
            _kind: PhantomData,
        }
    }
}

impl<'io, K, V, P: Planted<HandleTablePlant>> HandleTable<'io, K, V, P> {
    /// Creates a handle table with an explicit planted-bug seam for DST controls.
    pub fn with_plant(opts: HandleTableOpts<'io, 'io>, plant: P) -> Self {
        Self {
            brand: opts.brand,
            max_slots: opts.max_slots,
            slots: Vec::new_in(opts.alloc),
            free_list: Vec::new_in(opts.alloc),
            live_count: 0,
            retired_count: 0,
            planted_task_slots: Vec::new_in(opts.alloc),
            plant,
            _kind: PhantomData,
        }
    }

    #[inline]
    pub const fn brand(&self) -> Brand<'io> {
        self.brand
    }

    #[inline]
    pub const fn live_count(&self) -> u32 {
        self.live_count
    }

    #[inline]
    pub const fn retired_count(&self) -> u32 {
        self.retired_count
    }

    /// Inserts `value` owned by `owner`, issuing a deterministic branded [`Handle<'io, K>`].
    pub fn insert(&mut self, owner: TaskId, value: V) -> Result<Handle<'io, K>, HandleError> {
        self.insert_with_initial_gen(owner, value, NonZeroU32::MIN)
    }

    /// Inserts `value`, seeding a newly allocated slot at `initial_gen` (used by tests to
    /// exercise `u32::MAX` generation retirement without 4 billion iterations).
    pub(crate) fn insert_with_initial_gen(
        &mut self,
        owner: TaskId,
        value: V,
        initial_gen: NonZeroU32,
    ) -> Result<Handle<'io, K>, HandleError> {
        if let Some(slot_idx) = self.free_list.pop() {
            let entry = &mut self.slots[slot_idx as usize];
            debug_assert!(matches!(entry.state, SlotState::Vacant));
            entry.state = SlotState::Live { owner, value };
            self.live_count += 1;
            if self.plant.has(HandleTablePlant::ReclaimStaleSlotWithoutOwnerCheck) {
                self.planted_task_slots.push((owner, slot_idx));
            }
            return Ok(Handle::new(slot_idx, entry.generation, self.brand));
        }

        let slot_idx = u32::try_from(self.slots.len())
            .map_err(|_| HandleError::TableFull { max_slots: self.max_slots })?;
        if slot_idx >= self.max_slots {
            return Err(HandleError::TableFull { max_slots: self.max_slots });
        }
        self.slots.push(Slot {
            generation: initial_gen,
            state: SlotState::Live { owner, value },
        });
        self.live_count += 1;
        if self.plant.has(HandleTablePlant::ReclaimStaleSlotWithoutOwnerCheck) {
            self.planted_task_slots.push((owner, slot_idx));
        }
        Ok(Handle::new(slot_idx, initial_gen, self.brand))
    }

    /// Fast-forwards a live slot's generation to `u32::MAX` (test-only helper so DSTs can cover
    /// the `u32::MAX` retirement boundary after earlier generations exist in the stale set,
    /// without iterating 4 billion times).
    #[cfg(test)]
    pub(crate) fn fast_forward_gen_to_max(&mut self, handle: Handle<'io, K>) -> Option<Handle<'io, K>> {
        let entry = self.slots.get_mut(handle.slot as usize)?;
        if matches!(entry.state, SlotState::Live { .. }) && entry.generation == handle.generation {
            let max = NonZeroU32::new(u32::MAX).unwrap();
            entry.generation = max;
            return Some(Handle::new(handle.slot, max, self.brand));
        }
        None
    }

    /// Resolves `handle` immutably, rejecting stale or invalid handles by name.
    pub fn get(&self, handle: Handle<'io, K>) -> Result<&V, HandleError> {
        let Some(entry) = self.slots.get(handle.slot as usize) else {
            return Err(HandleError::InvalidSlot { slot: handle.slot });
        };
        match &entry.state {
            SlotState::Live { value, .. } if entry.generation == handle.generation => Ok(value),
            SlotState::Live { .. } => {
                Err(HandleError::Stale { slot: handle.slot, expected: Some(entry.generation), found: handle.generation })
            }
            SlotState::Vacant | SlotState::Retired => {
                Err(HandleError::Stale { slot: handle.slot, expected: None, found: handle.generation })
            }
        }
    }

    /// Resolves `handle` mutably, rejecting stale or invalid handles by name.
    pub fn get_mut(&mut self, handle: Handle<'io, K>) -> Result<&mut V, HandleError> {
        let Some(entry) = self.slots.get_mut(handle.slot as usize) else {
            return Err(HandleError::InvalidSlot { slot: handle.slot });
        };
        match &mut entry.state {
            SlotState::Live { value, .. } if entry.generation == handle.generation => Ok(value),
            SlotState::Live { .. } => {
                Err(HandleError::Stale { slot: handle.slot, expected: Some(entry.generation), found: handle.generation })
            }
            SlotState::Vacant | SlotState::Retired => {
                Err(HandleError::Stale { slot: handle.slot, expected: None, found: handle.generation })
            }
        }
    }

    /// Removes and returns the value for `handle`, verifying ownership and bumping (or retiring
    /// at `u32::MAX`) the slot's generation.
    pub fn remove(&mut self, handle: Handle<'io, K>, caller: TaskId) -> Result<V, HandleError> {
        let Some(entry) = self.slots.get_mut(handle.slot as usize) else {
            return Err(HandleError::InvalidSlot { slot: handle.slot });
        };
        match &entry.state {
            SlotState::Live { owner, .. } if entry.generation == handle.generation => {
                if *owner != caller {
                    return Err(HandleError::WrongOwner { slot: handle.slot, owner: *owner, caller });
                }
            }
            SlotState::Live { .. } => {
                return Err(HandleError::Stale { slot: handle.slot, expected: Some(entry.generation), found: handle.generation });
            }
            SlotState::Vacant | SlotState::Retired => {
                return Err(HandleError::Stale { slot: handle.slot, expected: None, found: handle.generation });
            }
        }
        let value = self.release_slot(handle.slot);
        Ok(value)
    }

    fn release_slot(&mut self, slot_idx: u32) -> V {
        let entry = &mut self.slots[slot_idx as usize];
        let old_gen = entry.generation.get();
        let (next_gen, next_state, push_free) = match old_gen.checked_add(1).and_then(NonZeroU32::new) {
            Some(g) => (g, SlotState::Vacant, true),
            None => {
                if self.plant.has(HandleTablePlant::WrapGenerationOnMax) {
                    (NonZeroU32::MIN, SlotState::Vacant, true)
                } else {
                    self.retired_count += 1;
                    (entry.generation, SlotState::Retired, false)
                }
            }
        };
        entry.generation = next_gen;
        let prev = core::mem::replace(&mut entry.state, next_state);
        self.live_count -= 1;
        if push_free {
            self.free_list.push(slot_idx);
        }
        match prev {
            SlotState::Live { value, .. } => value,
            _ => unreachable!("release_slot called on non-live slot"),
        }
    }

    /// Reclaims every handle currently owned by `task`.
    ///
    /// - On [`ExitKind::Cancelled`]: returns `Ok(reclaimed_count)`.
    /// - On [`ExitKind::Normal`]: returns `Ok(0)` if the task closed everything cleanly, or
    ///   `Err(LeakedHandles { task, count })` if the driver had to reclaim any leaked handle.
    pub fn reclaim_task(
        &mut self,
        task: TaskId,
        exit: ExitKind,
        mut on_reclaim: impl FnMut(V),
    ) -> Result<u32, LeakedHandles> {
        let mut reclaimed: u32 = 0;

        if self.plant.has(HandleTablePlant::ReclaimStaleSlotWithoutOwnerCheck) {
            // Planted bug: blindly close every slot `task` ever owned, even if reused by a sibling.
            for i in 0..self.planted_task_slots.len() {
                let (recorded_task, slot_idx) = self.planted_task_slots[i];
                if recorded_task == task && matches!(self.slots[slot_idx as usize].state, SlotState::Live { .. }) {
                    let v = self.release_slot(slot_idx);
                    on_reclaim(v);
                    reclaimed += 1;
                }
            }
        } else {
            for slot_idx in 0..self.slots.len() {
                let owned_by_task = matches!(
                    self.slots[slot_idx].state,
                    SlotState::Live { owner, .. } if owner == task
                );
                if owned_by_task {
                    let v = self.release_slot(slot_idx as u32);
                    on_reclaim(v);
                    reclaimed += 1;
                }
            }
        }

        match exit {
            ExitKind::Cancelled => Ok(reclaimed),
            ExitKind::Normal if reclaimed == 0 => Ok(0),
            ExitKind::Normal => Err(LeakedHandles { task, count: reclaimed }),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU32;

    use blaze_own::testing::heap;
    use blaze_own::{Alloc, Vec, with_brand};
    use blaze_sim::{Broke, Config};

    use super::{
        ExitKind, FileKind, Handle, HandleError, HandleTable, HandleTableOpts, HandleTablePlant,
        LeakedHandles, TaskId,
    };

    /// The invariant: across arbitrary interleaved opens, reads, closes, and task cancellations
    /// over 4 concurrent tasks, every live handle resolves to its exact value, every closed or
    /// reclaimed handle is rejected as stale, sibling tasks' handles are never disturbed on
    /// cancellation, and a normal exit with unclosed handles fails with `LeakedHandles`.
    fn run_handle_table_dst(plant: HandleTablePlant) {
        let h = heap();
        blaze_sim::check(Config::cases(200), |tape| {
            let mut result: Result<(), Broke> = Ok(());
            with_brand(|brand| {
                let alloc = h.alloc();
                let mut table: HandleTable<'_, FileKind, u64, HandleTablePlant> =
                    HandleTable::with_plant(
                        HandleTableOpts { brand, max_slots: 16, alloc },
                        plant,
                    );
                let mut live: Vec<(TaskId, Handle<'_, FileKind>, u64), Alloc<'_>> =
                    Vec::new_in(alloc);
                let mut dead: Vec<Handle<'_, FileKind>, Alloc<'_>> = Vec::new_in(alloc);
                let mut next_val: u64 = 1;

                let ops = tape.below(48, "ops");
                for _ in 0..ops {
                    let task = TaskId(tape.below(4, "task") as u32);
                    match tape.below(6, "op") {
                        0 => {
                            // Open a handle for `task`.
                            let val = next_val;
                            next_val += 1;
                            match table.insert(task, val) {
                                Ok(hdl) => {
                                    // Stale handles must never collide with a newly issued handle.
                                    if dead.iter().any(|&old| old == hdl) {
                                        result = Err(Broke::Oracle(
                                            "generation wraparound: newly issued handle equalled a retired/stale handle".into(),
                                        ));
                                        return;
                                    }
                                    live.push((task, hdl, val));
                                }
                                Err(HandleError::TableFull { .. }) => {}
                                Err(e) => {
                                    result = Err(Broke::Oracle(format!("unexpected insert error: {e}")));
                                    return;
                                }
                            }
                        }
                        1 => {
                            // Close one live handle properly by its owner.
                            if !live.is_empty() {
                                let idx = (tape.below(live.len() as u64, "close_idx") as usize)
                                    .min(live.len() - 1);
                                let (owner, hdl, val) = live.swap_remove(idx);
                                match table.remove(hdl, owner) {
                                    Ok(got) if got == val => dead.push(hdl),
                                    Ok(got) => {
                                        result = Err(Broke::Oracle(format!(
                                            "close returned {got}, expected {val}"
                                        )));
                                        return;
                                    }
                                    Err(e) => {
                                        result = Err(Broke::Oracle(format!(
                                            "live handle close failed: {e}"
                                        )));
                                        return;
                                    }
                                }
                            }
                        }
                        2 => {
                            // Cancel `task`: reclaim all of `task`'s handles, verify siblings untouched.
                            let mut expected_reclaimed = 0u32;
                            let mut kept: Vec<(TaskId, Handle<'_, FileKind>, u64), Alloc<'_>> =
                                Vec::new_in(alloc);
                            for &(owner, hdl, val) in live.as_slice() {
                                if owner == task {
                                    expected_reclaimed += 1;
                                    dead.push(hdl);
                                } else {
                                    kept.push((owner, hdl, val));
                                }
                            }
                            live = kept;
                            let mut actual = 0u32;
                            match table.reclaim_task(task, ExitKind::Cancelled, |_| actual += 1) {
                                Ok(n) if n == expected_reclaimed && actual == expected_reclaimed => {}
                                other => {
                                    result = Err(Broke::Oracle(format!(
                                        "reclaim_task({task:?}) mismatch: expected {expected_reclaimed}, got {other:?}"
                                    )));
                                    return;
                                }
                            }
                        }
                        3 => {
                            // Normal exit check: must fail with `LeakedHandles` iff task still holds handles.
                            let mut task_owned = 0u32;
                            let mut kept: Vec<(TaskId, Handle<'_, FileKind>, u64), Alloc<'_>> =
                                Vec::new_in(alloc);
                            for &(owner, hdl, val) in live.as_slice() {
                                if owner == task {
                                    task_owned += 1;
                                    dead.push(hdl);
                                } else {
                                    kept.push((owner, hdl, val));
                                }
                            }
                            live = kept;
                            let res = table.reclaim_task(task, ExitKind::Normal, |_| {});
                            if task_owned == 0 && res != Ok(0) {
                                result = Err(Broke::Oracle(format!(
                                    "clean normal exit reported {res:?}"
                                )));
                                return;
                            }
                            if task_owned > 0 && res != Err(LeakedHandles { task, count: task_owned }) {
                                result = Err(Broke::Oracle(format!(
                                    "leaked normal exit expected Err({task_owned}), got {res:?}"
                                )));
                                return;
                            }
                        }
                        4 => {
                            // Fast-forward one live slot to u32::MAX (simulating 4B reuses after
                            // earlier generations are already in `dead`) to test slot retirement.
                            if !live.is_empty() {
                                let idx = (tape.below(live.len() as u64, "ff_idx") as usize)
                                    .min(live.len() - 1);
                                let (owner, hdl, val) = live[idx];
                                if let Some(max_hdl) = table.fast_forward_gen_to_max(hdl) {
                                    live[idx] = (owner, max_hdl, val);
                                }
                            }
                        }
                        _ => {
                            // Oracle sweep: every live handle reads its exact value, every dead handle is stale.
                            for &(_, hdl, val) in live.as_slice() {
                                if table.get(hdl) != Ok(&val) {
                                    result = Err(Broke::Oracle(format!(
                                        "live handle {hdl:?} corrupted or reclaimed: got {:?}, expected {val}",
                                        table.get(hdl)
                                    )));
                                    return;
                                }
                            }
                            for &old in dead.as_slice() {
                                if table.get(old).is_ok() {
                                    result = Err(Broke::Oracle(format!(
                                        "stale handle {old:?} unexpectedly resolved!"
                                    )));
                                    return;
                                }
                            }
                        }
                    }
                }
                if table.live_count() != live.len() as u32 {
                    result = Err(Broke::Oracle(format!(
                        "live_count {} != model {}",
                        table.live_count(),
                        live.len()
                    )));
                }
            });
            result
        });
    }

    #[test]
    fn handle_table_multi_task_interleaving_and_reclamation_dst() {
        run_handle_table_dst(HandleTablePlant::None);
    }

    #[test]
    #[should_panic(expected = "generation wraparound")]
    fn the_oracle_catches_generation_wraparound() {
        run_handle_table_dst(HandleTablePlant::WrapGenerationOnMax);
    }

    #[test]
    #[should_panic(expected = "reclaim_task")]
    fn the_oracle_catches_reclaim_touching_reused_slot() {
        run_handle_table_dst(HandleTablePlant::ReclaimStaleSlotWithoutOwnerCheck);
    }

    #[test]
    fn deterministic_slot_allocation_replays_identically() {
        let h = heap();
        let run_once = || {
            let mut slots = Vec::new_in(h.alloc());
            with_brand(|brand| {
                let mut table: HandleTable<'_, FileKind, u32> =
                    HandleTable::new(HandleTableOpts { brand, max_slots: 8, alloc: h.alloc() });
                let a = table.insert(TaskId(1), 10).unwrap();
                let b = table.insert(TaskId(1), 20).unwrap();
                table.remove(a, TaskId(1)).unwrap();
                let c = table.insert(TaskId(2), 30).unwrap();
                slots.push((a.slot(), a.generation().get()));
                slots.push((b.slot(), b.generation().get()));
                slots.push((c.slot(), c.generation().get()));
                // `c` deterministically reuses slot 0 with generation 2.
                assert_eq!((c.slot(), c.generation().get()), (0, 2));
                assert!(matches!(table.get(a), Err(HandleError::Stale { slot: 0, .. })));
            });
            slots
        };
        assert_eq!(run_once().as_slice(), run_once().as_slice());
    }
}
