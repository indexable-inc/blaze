//! The requests in flight, as columns (docs/v2/plan/design/data.md): a slab of 32-byte [`Hot`]
//! records, what a completion reads (the kind, the tag, the progress, whose request it is),
//! and beside it by the same slot the buffer the request lends the kernel and the [`Cold`]
//! fields only its submission and its end read. A request is named by its slab handle, whose
//! bits are its completions' `user_data` (`super::data`): the generation in the high half, so a
//! completion of a request that ended never names the request now in its slot.
//!
//! Bytes per request: the hot record and the slab's 8 of generation and free list (40), the
//! buffer's `Vec` header (24), the cold record (88): 152, against the 170-odd bytes of the enum
//! it replaced, which every completion moved out of its slot and back. A completion reads the
//! 40 and, for a read or a send it ends, the 24: one cache line, two where a record straddles
//! one. A lookup is a bounds check, one compare of the generation and a load.
//!
//! The names requests on paths hand the kernel (and the `statx` and descriptor it reads or
//! writes beside them) are in [`Paths`], two arenas used in turn, and the registered buffers of writes that bypass the page cache are lent by [`Lent`],
//! in the order they were asked for.

use std::{collections::VecDeque, ffi::CStr, mem, os::fd::OwnedFd};

use blaze_mem::{Handle, MemError, RegionVec, Slab, Tag as MemTag};

use crate::{FileName, Tag};

/// What a request is, the `u8` tag its [`Hot::kind`] dispatches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum Kind {
    /// Into its buffer from [`Cold::at`], [`Hot::done`] of it read; [`Hot::of`] its kept file.
    /// With [`WHOLE`] the whole file, its buffer sized once the file is open. The kernel reads
    /// into the buffer's spare capacity, after its length, which counts what is read.
    Read,
    /// A read of a file no request keeps, from [`Cold::at`] into its buffer's spare capacity:
    /// the file named first opened through the ring to a descriptor of its own
    /// ([`Cold::fds`] 0), with [`WHOLE`] sized by its `fstat`, read, and closed as it ends. So
    /// a read sees the file at its name now, as [`crate::threads::Threads`] does, never one a
    /// descriptor kept from before a rename over it still names.
    Load,
    /// Its buffer at the end of its kept file ([`Hot::of`]).
    Append,
    /// Its buffer at [`Cold::at`] of the file named first, opened into a slot of its own
    /// ([`Cold::slots`] 0) by the first link of its chain. With [`DIRECT`] the descriptor
    /// bypasses the page cache, and each piece is copied into the registered buffer
    /// [`Hot::buffer`] and written from there; a `statx` first says whether it may.
    WriteAt,
    /// Its kept file ([`Hot::of`]) synced.
    Sync,
    /// A whole file as one linked chain: its directory (named third) opened into slot 1 and
    /// the temporary (named first) made into slot 0, written, synced, renamed over the path
    /// (named second) and the directory synced. A short write asks the chain again from the
    /// write; one that fails after the temporary was made unlinks it.
    Write,
    /// The first name renamed over the second, linked after the opens of the directories of
    /// each (named third and fourth) and before their syncs.
    Rename,
    /// A kept file ([`Hot::of`]) opened through the ring, then its descriptor put in its slot
    /// of the table ([`Cold::slots`] 0), and the requests waiting for it asked.
    Open,
    /// Empties slot [`Hot::of`] of the table; its own completion is nobody's.
    Close,
    /// The file or empty directory named first unlinked.
    Remove,
    /// The first name linked as the second.
    Link,
    /// The second name made sharing the first's blocks: both opened through the ring
    /// ([`Cold::fds`]), then `FICLONE`.
    Clone,
    /// The file named first opened through the ring and mapped.
    Map,
    /// The `statx` of the name first, into the arena after its names.
    Stat,
    /// Its kept file ([`Hot::of`]) cut to [`Cold::at`] bytes once it is open.
    Truncate,
    /// A read of a watch's events; [`Hot::of`] its watching.
    Watch,
    /// Stops the request whose `user_data` is [`Hot::of`]; its own completion is nobody's.
    Cancel,
    /// A poll of a spawn's pidfd; [`Hot::of`] the spawn, which keeps the pidfd open.
    Exit,
    /// One of a spawn's output pipes ([`Cold::fds`] 0), read into its buffer until it ends;
    /// [`STDERR`] for the second.
    Output,
    /// A spawn's input, its buffer, written to its pipe ([`Cold::fds`] 0), closed when it ends.
    Input,
    /// A listen's multishot accept; [`Hot::of`] its listener.
    Accept,
    /// [`Hot::of`] its connecting, whose address the kernel reads until it completes.
    Connect,
    /// A connection's multishot receive into the ring of provided buffers; [`Hot::of`] the
    /// connection's bits.
    Recv,
    /// Its buffer sent on the connection [`Hot::of`]; with [`ZERO_COPY`], from the asker's
    /// pages, kept until the kernel's notification of each send says it read them.
    Send,
    Shutdown,
    /// A read of the eventfd work off the ring wakes it with, into the driver's one count.
    Wake,
}

/// Given to the kernel before the ring failed: kept where it is, never freed or asked again.
pub(super) const ABANDONED: u8 = 1 << 0;
/// A listen being stopped.
pub(super) const STOPPING: u8 = 1 << 1;
/// A write at an offset that bypasses the page cache.
pub(super) const DIRECT: u8 = 1 << 2;
/// The output pipe read is the spawn's standard error.
pub(super) const STDERR: u8 = 1 << 3;
/// Its names are in the second of [`Paths`]' arenas.
pub(super) const SECOND: u8 = 1 << 4;
/// A send from the asker's pages (`IORING_OP_SEND_ZC`).
pub(super) const ZERO_COPY: u8 = 1 << 5;
/// A zero-copy send whose last result is in, ended once its notifications are.
pub(super) const FINISHED: u8 = 1 << 6;
/// A read of the whole file (a [`Kind::Read`]'s or a [`Kind::Load`]'s; the bit an output's
/// [`STDERR`] is).
pub(super) const WHOLE: u8 = 1 << 3;
/// A request on a path that makes the directories it is in where they are missing (the bit a
/// send's [`ZERO_COPY`] is).
pub(super) const MAKE: u8 = 1 << 5;
/// A request on a path whose missing directories were made once: a second `ENOENT` fails it.
pub(super) const MADE: u8 = 1 << 7;

/// In [`Hot::buffer`]: none.
pub(super) const NO_BUFFER: u16 = u16::MAX;
/// In [`Cold::slots`]: none.
pub(super) const NO_SLOT: u32 = u32::MAX;

/// The buffer of a request that lends the kernel none.
// alloc: none, an empty `Vec` holds no memory.
pub(super) const NO_BYTES: Vec<u8> = Vec::new();

/// What a completion reads of its request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(super) struct Hot {
    pub tag: Tag,
    /// Bytes read, written or sent so far.
    pub done: u64,
    /// Whose request it is: the bits of its kept file's, watching's, spawn's, listener's or
    /// connecting's handle, or its connection's [`crate::Conn`]; a cancel's target `user_data`.
    pub of: u64,
    pub kind: Kind,
    /// [`ABANDONED`], [`STOPPING`], [`DIRECT`], [`STDERR`], [`SECOND`], [`ZERO_COPY`],
    /// [`FINISHED`], [`WHOLE`], [`MAKE`], [`MADE`].
    pub flags: u8,
    /// The links asked of the kernel whose last completion is not in yet, a bit each.
    pub links: u8,
    /// Which of its steps a request on a path is at (`super::files`); 0 for any other.
    pub phase: u8,
    /// Notifications of zero-copy sends the kernel still owes it: one per send asked, so a
    /// send cut short many times owes many at once.
    pub notifs: u16,
    /// Its registered buffer, or [`NO_BUFFER`].
    pub buffer: u16,
}

const _: () = assert!(size_of::<Hot>() == 32);

impl Hot {
    pub(super) const fn new(tag: Tag, kind: Kind, of: u64) -> Hot {
        Hot { tag, done: 0, of, kind, flags: 0, links: 0, phase: 0, notifs: 0, buffer: NO_BUFFER }
    }
}

/// What only a request's submission and its end read. The kernel reads and writes none of it:
/// what it is handed by address is in the request's arena of [`Paths`].
pub(super) struct Cold {
    /// The offset of a read or a write at one, the length a truncate cuts to; for a request
    /// making directories, how far into its path they are made; for a whole file's write
    /// unlinking its temporary, why it failed.
    pub at: u64,
    /// The descriptors it owns, closed when it ends: a spawn's pipe, a clone's two files, a
    /// map's file, a kept file's until it is put in the table.
    pub fds: [Option<OwnedFd>; 2],
    /// The slots of the table its opens put files in, each emptied through the ring as it ends.
    pub slots: [u32; 2],
    /// Which of [`Cold::slots`] an open filled.
    pub opened: [bool; 2],
    /// Where each of its NUL-terminated names starts in its arena of [`Paths`]; the fifth is a
    /// directory being made, or the bytes the kernel writes or reads beside the names (a
    /// `statx`, a descriptor to put in the table).
    pub names: [u32; 5],
    /// The results of its chain's links, by link. A zero-copy send keeps in the first why it
    /// failed (an errno; 0 for none) until its notifications are in.
    pub results: [i32; LINKS],
    /// The name a whole file's write makes another file, forgotten as it ends.
    pub file: Option<FileName>,
}

/// The most links a request's chain has: a whole file's write's two opens, write, sync,
/// rename and sync of its directory.
pub(super) const LINKS: usize = 6;

const _: () = assert!(size_of::<Cold>() == 88);

impl Cold {
    pub(super) const EMPTY: Cold = Cold {
        at: 0,
        fds: [None, None],
        slots: [NO_SLOT; 2],
        opened: [false; 2],
        names: [0; 5],
        results: [0; LINKS],
        file: None,
    };
}

pub(super) type Id = Handle<Hot>;

/// The requests in flight or waiting, in columns by slot. The columns are in reserved memory
/// that never moves, so a buffer's header and a cold record stay where the kernel was told
/// they are until the request ends.
pub(super) struct Ops {
    hot: Slab<Hot>,
    bufs: RegionVec<Vec<u8>>,
    cold: RegionVec<Cold>,
}

impl Ops {
    pub(super) fn new(max: u32) -> Result<Ops, MemError> {
        Ok(Ops {
            hot: Slab::new(max, MemTag::Io)?,
            bufs: RegionVec::new(max as usize, MemTag::Io)?,
            cold: RegionVec::new(max as usize, MemTag::Io)?,
        })
    }

    pub(super) fn insert(&mut self, hot: Hot, buf: Vec<u8>, cold: Cold) -> Id {
        let id = self.hot.insert(hot);
        let at = id.index() as usize;
        // A slab grows by one slot at a time, so a new slot is the columns' next.
        debug_assert!(at <= self.bufs.len());
        if at == self.bufs.len() {
            self.bufs.push(buf);
            self.cold.push(cold);
        } else {
            self.bufs[at] = buf;
            self.cold[at] = cold;
        }
        id
    }

    #[inline]
    pub(super) fn get(&self, id: Id) -> Option<&Hot> {
        self.hot.get(id)
    }

    #[inline]
    pub(super) fn get_mut(&mut self, id: Id) -> Option<&mut Hot> {
        self.hot.get_mut(id)
    }

    pub(super) fn contains(&self, id: Id) -> bool {
        self.hot.contains(id)
    }

    pub(super) fn len(&self) -> usize {
        self.hot.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.hot.is_empty()
    }

    /// The buffer of `id`, which is in flight.
    #[inline]
    pub(super) fn buf(&self, id: Id) -> &Vec<u8> {
        debug_assert!(self.contains(id));
        &self.bufs[id.index() as usize]
    }

    #[inline]
    pub(super) fn buf_mut(&mut self, id: Id) -> &mut Vec<u8> {
        debug_assert!(self.contains(id));
        &mut self.bufs[id.index() as usize]
    }

    #[inline]
    pub(super) fn cold(&self, id: Id) -> &Cold {
        debug_assert!(self.contains(id));
        &self.cold[id.index() as usize]
    }

    #[inline]
    pub(super) fn cold_mut(&mut self, id: Id) -> &mut Cold {
        debug_assert!(self.contains(id));
        &mut self.cold[id.index() as usize]
    }

    /// `id` taken out with its buffer and cold record; every handle to it is stale from now on.
    pub(super) fn remove(&mut self, id: Id) -> Option<(Hot, Vec<u8>, Cold)> {
        let hot = self.hot.remove(id)?;
        let at = id.index() as usize;
        Some((hot, mem::take(&mut self.bufs[at]), mem::replace(&mut self.cold[at], Cold::EMPTY)))
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (Id, &Hot)> {
        self.hot.iter()
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (Id, &mut Hot)> {
        self.hot.iter_mut()
    }
}

/// The names in flight, NUL-terminated, in two arenas of reserved memory used in turn: names
/// go into the current one; once it holds [`TURN`] bytes and no request holds a name of the
/// other, the other is emptied and becomes current. An arena no request holds a name of is
/// emptied as it is next used, so a driver whose writes come and go reuses its first pages.
/// An arena never moves as it grows, so a name stays where the kernel was told it is.
pub(super) struct Paths {
    arenas: [RegionVec<u8>; 2],
    /// Requests holding names of each.
    live: [u32; 2],
    current: u8,
}

/// The most bytes of names an arena holds: a write's two names are at most `2 * PATH_MAX`,
/// so this is 8192 writes' at the longest and some 300 000 at a typical 200 bytes.
const NAMES: usize = 64 << 20;
/// An arena's size past which the next names go into the other, once it is free.
const TURN: usize = 1 << 20;

impl Paths {
    pub(super) fn new() -> Result<Paths, MemError> {
        Ok(Paths {
            arenas: [RegionVec::new(NAMES, MemTag::Io)?, RegionVec::new(NAMES, MemTag::Io)?],
            live: [0; 2],
            current: 0,
        })
    }

    /// The arena the next names go into.
    pub(super) fn begin(&mut self) -> u8 {
        let (now, other) = (self.current as usize, 1 - self.current as usize);
        if self.live[now] == 0 {
            self.arenas[now].clear();
        } else if self.arenas[now].len() >= TURN && self.live[other] == 0 {
            self.arenas[other].clear();
            self.current = other as u8;
        }
        self.current
    }

    /// The arena names are pushed into. Never read as a slice: the kernel writes into bytes of
    /// it (a request's `statx`) while other requests' names are read.
    pub(super) fn arena(&mut self, arena: u8) -> &mut RegionVec<u8> {
        &mut self.arenas[arena as usize]
    }

    /// The address of byte `at` of `arena`, which is below its length.
    pub(super) fn at(&self, arena: u8, at: u32) -> *mut u8 {
        self.arenas[arena as usize].as_ptr_at(at as usize).expect("a name of a request in flight").as_ptr()
    }

    /// The NUL-terminated name at `at` of `arena`, which a request put there.
    ///
    /// # Safety
    ///
    /// `at` starts a name pushed NUL-terminated (`push_c` and the like) into `arena` by a
    /// request that still holds it, never a `statx` room: the scan runs to the first NUL, so
    /// from anywhere else it could run into bytes the kernel is writing, or past the arena.
    pub(super) unsafe fn name(&self, arena: u8, at: u32) -> &CStr {
        // SAFETY: as the caller says, `at` starts a NUL-terminated name the arena keeps until
        // its request ends (it is cleared only once none holds it); the kernel only reads
        // names, so nothing writes the bytes scanned while this borrows them.
        unsafe { CStr::from_ptr(self.at(arena, at).cast()) }
    }

    /// A request holds names of `arena` until [`Paths::release`].
    pub(super) fn hold(&mut self, arena: u8) {
        self.live[arena as usize] += 1;
    }

    pub(super) fn release(&mut self, arena: u8) {
        self.live[arena as usize] -= 1;
    }

    /// Requests holding names, in each arena.
    #[cfg(test)]
    pub(super) fn held(&self) -> [u32; 2] {
        self.live
    }
}

/// Which of [`Paths`]' arenas a request's [`Hot::flags`] name.
pub(super) fn arena_of(hot: &Hot) -> u8 {
    u8::from(hot.flags & SECOND != 0)
}

/// The registered buffers lent to writes that bypass the page cache, one each: a write asks
/// for one as it is queued, and one that finds none free waits, in the order it asked, for the
/// next given back. Never an error: a buffer is given back by every such write as it ends.
pub(super) struct Lent {
    free: Vec<u16>,
    waiting: VecDeque<Id>,
}

impl Lent {
    pub(super) fn new(count: u16) -> Lent {
        // alloc: setup, once per driver.
        Lent { free: (0..count).rev().collect(), waiting: VecDeque::with_capacity(super::ENTRIES as usize) }
    }

    /// A buffer for `id`, or none, `id` then waiting for the next given back.
    pub(super) fn take(&mut self, id: Id) -> Option<u16> {
        let buffer = self.free.pop();
        if buffer.is_none() {
            self.waiting.push_back(id);
        }
        buffer
    }

    /// `buffer` given back: to the request that has waited longest and is still `live`, or
    /// kept free.
    pub(super) fn give(&mut self, buffer: u16, live: impl Fn(Id) -> bool) -> Option<(Id, u16)> {
        while let Some(id) = self.waiting.pop_front() {
            if live(id) {
                return Some((id, buffer));
            }
        }
        self.free.push(buffer);
        None
    }

    pub(super) fn free(&self) -> usize {
        self.free.len()
    }

    pub(super) fn waiters(&self) -> impl Iterator<Item = Id> + '_ {
        self.waiting.iter().copied()
    }

    /// Forgets every request waiting: none of them is given to the kernel.
    pub(super) fn clear_waiting(&mut self) -> std::collections::vec_deque::Drain<'_, Id> {
        self.waiting.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use blaze_hash::HashMap;

    use super::*;

    /// The columns hold what was put in a slot, with the hot record, and a removed slot's
    /// buffer and cold record go with it, over random starts and ends of many seeds.
    #[test]
    fn columns_move_with_their_slot() {
        for seed in 0..32 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut ops = Ops::new(1 << 12).unwrap();
            let mut model: HashMap<u64, (Id, u64, Vec<u8>, u64)> = HashMap::default();
            for next in 0..2_000u64 {
                if model.is_empty() || (model.len() < 128 && rng.below(3) != 0) {
                    let bytes = next.to_le_bytes().to_vec();
                    let cold = Cold { at: next * 3, ..Cold::EMPTY };
                    let id = ops.insert(Hot::new(Tag(next), Kind::Read, next * 5), bytes.clone(), cold);
                    model.insert(next, (id, next * 5, bytes, next * 3));
                } else {
                    let key = *model.keys().nth(rng.below(model.len() as u64) as usize).unwrap();
                    let (id, of, bytes, at) = model.remove(&key).unwrap();
                    let (hot, buf, cold) = ops.remove(id).expect("live");
                    assert_eq!((hot.tag, hot.of, buf, cold.at), (Tag(key), of, bytes, at));
                    assert!(ops.get(id).is_none() && ops.remove(id).is_none());
                }
                assert_eq!(ops.len(), model.len());
            }
            for (key, (id, of, bytes, at)) in &model {
                let hot = ops.get(*id).expect("live");
                assert_eq!((hot.tag, hot.of, ops.buf(*id), ops.cold(*id).at), (Tag(*key), *of, bytes, *at));
            }
        }
    }

    /// An arena is emptied once no request holds its names, and new names go to the other
    /// once the current one is past its turn and the other is free.
    #[test]
    fn names_take_turns() {
        let mut names = Paths::new().unwrap();
        let first = names.begin();
        names.arena(first).extend_from_slice(&[1; TURN]);
        names.hold(first);
        let second = names.begin();
        assert_ne!(first, second, "past its turn, with the other free");
        assert!(names.arena(second).is_empty());
        names.arena(second).extend_from_slice(b"a\0");
        names.hold(second);
        assert_eq!(names.begin(), second, "the first is still held");
        names.release(first);
        names.release(second);
        assert_eq!(names.held(), [0, 0]);
        let again = names.begin();
        assert!(names.arena(again).is_empty(), "nothing held: emptied as it is next used");
    }

    /// More writes than buffers: each that finds none free waits, and each buffer given back
    /// goes to the one that waited longest and is still in flight, never failing one.
    #[test]
    fn a_write_without_a_free_buffer_waits_for_the_next() {
        let mut ops = Ops::new(64).unwrap();
        let ids: Vec<Id> =
            (0..12).map(|n| ops.insert(Hot::new(Tag(n), Kind::WriteAt, 0), Vec::new(), Cold::EMPTY)).collect();
        let mut lent = Lent::new(4);
        let mut holding: VecDeque<(Id, u16)> = VecDeque::new();
        for &id in &ids {
            if let Some(buffer) = lent.take(id) {
                holding.push_back((id, buffer));
            }
        }
        assert_eq!((holding.len(), lent.free(), lent.waiters().count()), (4, 0, 8));
        // One waiter ends before its turn: it is passed over.
        ops.remove(ids[5]);
        let mut served = Vec::new();
        while let Some((id, buffer)) = holding.pop_front() {
            served.push(id);
            ops.remove(id);
            if let Some(next) = lent.give(buffer, |it| ops.contains(it)) {
                holding.push_back(next);
            }
        }
        let wanted: Vec<Id> = ids.iter().copied().filter(|&it| it != ids[5]).collect();
        assert_eq!(served, wanted, "in the order they asked");
        assert_eq!((lent.free(), lent.waiters().count()), (4, 0));
    }
}
