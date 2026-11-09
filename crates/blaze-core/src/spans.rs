//! Memory an asker owns that effects read into and write from: [`crate::Effect::ReadInto`] and
//! [`crate::Effect::WriteFrom`] (docs/v2/plan/design/store-pool.md, "The effect").
//!
//! An asker registers a region of its memory once ([`Spans::register`]), and names a part of it
//! in an effect as a [`Span`]: data, not a pointer, so an [`crate::Effect`] stays `Clone + Eq +
//! Debug`, a test can log it, and [`crate::sim::Sim`] carries it out against the same memory a
//! real driver would. A driver turns a span into an address as it starts the effect, checking that
//! its region is registered (a stale one, unregistered since, is refused by its generation) and
//! that the span lies inside it.
//!
//! The registry is the process's: a fixed table of [`REGIONS`] slots, each a state word (its
//! generation, and whether it is free, being filled or live) beside the region's base and length,
//! so a lookup is two atomic loads and a compare, and no lock is taken on the IO path.
//!
//! The asker's contract, turned around from [`crate::Effect::Map`]'s: while a read into a span is
//! in flight, nothing reads, writes or gives back the span's bytes (they are the kernel's; after it
//! completes or fails they are the asker's again, holding what the completion says); while a write
//! from a span is in flight, nothing writes or gives back its bytes. The store's pool upholds it
//! with its pages' `Loading` state and its commit's pins. [`Spans::register`] is `unsafe`
//! because a driver writes into the memory through the address it was given, which the borrow
//! checker cannot see.
//!
//! Why unsafe here: a registered region is memory of the asker's that the drivers write into and
//! read from by address, outside any borrow. This module is the one place a span becomes an
//! address or a slice; the threads driver and the simulation reach the bytes through its safe
//! [`Resolved::with`] and [`Resolved::with_mut`], whose soundness is the registrant's promise.

use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    fmt,
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use crate::{Cache, FileName, IoError};

/// The most regions registered at once: the store's pool registers one, a test a few.
pub const REGIONS: usize = 64;

/// What a read or write that bypasses the page cache ([`Cache::Bypassed`]) must be a whole
/// number of: its file offset, its span's address and its span's length. 4 KiB: every NVMe
/// drive's logical block, Linux's `O_DIRECT` alignment on the file systems blaze runs on, and
/// NTFS's `FILE_FLAG_NO_BUFFERING` sector multiple. Asked of every driver, the simulation too,
/// so a misaligned asker fails in its tests as it would on Linux.
pub const DIRECT_ALIGN: u64 = 4096;

/// A region of memory registered with [`Spans::register`]: which slot of the table, and which of
/// the slot's registrations. Only its [`Registered`] guard makes one, so only the registrant can
/// hand out spans of its memory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SpanRegion {
    slot: u32,
    generation: u32,
}

/// `len` bytes at `offset` of a registered region, which a read fills or a write takes its bytes
/// from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Span {
    pub region: SpanRegion,
    pub offset: u64,
    pub len: u32,
}

impl Span {
    /// The offset one past its last byte, in its region.
    pub fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{} of {}", self.offset, self.end(), name_of(self.region))
    }
}

/// Free: no region; [`Spans::register`] may take it.
const FREE: u64 = 0;
/// Taken by a registration still writing its base, length and name.
const FILLING: u64 = 1;
/// A region's, looked up by spans.
const LIVE: u64 = 2;
const PHASE: u64 = 3;

/// One slot of the table: the state word (generation above the two bits of [`FREE`],
/// [`FILLING`] or [`LIVE`]), and the region's base, length and name, written while the slot is
/// [`FILLING`] and read only once its state is seen [`LIVE`] with an acquire load.
struct Slot {
    state: AtomicU64,
    base: AtomicUsize,
    len: AtomicUsize,
    name: AtomicUsize,
    name_len: AtomicUsize,
}

impl Slot {
    const EMPTY: Slot = Slot {
        state: AtomicU64::new(FREE),
        base: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
        name: AtomicUsize::new(0),
        name_len: AtomicUsize::new(0),
    };
}

static SLOTS: [Slot; REGIONS] = [const { Slot::EMPTY }; REGIONS];

fn live(generation: u32) -> u64 {
    u64::from(generation) << 2 | LIVE
}

/// The registry of regions effects may read into and write from.
pub struct Spans;

/// Every slot of the registry holds a region: [`Spans::register`] of `name` found none free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpansFull {
    pub name: &'static str,
}

impl fmt::Display for SpansFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "registering {} for reads into and writes from it: all {REGIONS} regions of the \
             registry are taken (blaze_core::spans::REGIONS)",
            self.name
        )
    }
}

impl std::error::Error for SpansFull {}

impl Spans {
    /// `len` bytes at `base`, named `name` in every error about them, registered for
    /// [`crate::Effect::ReadInto`] and [`crate::Effect::WriteFrom`] until the guard is dropped.
    ///
    /// # Safety
    ///
    /// The bytes `base..base + len` are the caller's, readable and writable, and stay so until the
    /// guard is dropped. The caller hands the guard's spans only to effects that keep the
    /// contract of the module's docs (nothing touches a read's span while it is in flight, nothing
    /// writes a write's span while it is in flight), and drops the guard only once no effect on a
    /// span of it is in flight. A driver writes into the bytes from another thread (the threads
    /// driver, the kernel), so a read of them that races a read in flight is a data race.
    pub unsafe fn register(
        name: &'static str,
        base: NonNull<u8>,
        len: usize,
    ) -> Result<Registered, SpansFull> {
        for (at, slot) in SLOTS.iter().enumerate() {
            let state = slot.state.load(Ordering::Relaxed);
            if state & PHASE != FREE {
                continue;
            }
            let generation = (state >> 2) as u32;
            let filling = u64::from(generation) << 2 | FILLING;
            if slot
                .state
                .compare_exchange(state, filling, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            slot.base.store(base.as_ptr() as usize, Ordering::Relaxed);
            slot.len.store(len, Ordering::Relaxed);
            slot.name.store(name.as_ptr() as usize, Ordering::Relaxed);
            slot.name_len.store(name.len(), Ordering::Relaxed);
            slot.state.store(live(generation), Ordering::Release);
            let region = SpanRegion { slot: at as u32, generation };
            return Ok(Registered { region, len, name });
        }
        Err(SpansFull { name })
    }

    /// How many regions are registered now.
    pub fn registered() -> usize {
        SLOTS.iter().filter(|it| it.state.load(Ordering::Relaxed) & PHASE == LIVE).count()
    }
}

/// A region registered by [`Spans::register`], unregistered when dropped: a span of it in an
/// effect asked after that fails, naming it.
#[derive(Debug)]
pub struct Registered {
    region: SpanRegion,
    len: usize,
    name: &'static str,
}

impl Registered {
    pub fn region(&self) -> SpanRegion {
        self.region
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// `len` bytes at `offset` of the region. Not checked here: a driver refuses a span past
    /// the region's end, naming it.
    pub fn span(&self, offset: u64, len: u32) -> Span {
        Span { region: self.region, offset, len }
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let slot = &SLOTS[self.region.slot as usize];
        // The next generation, free: a span of this one names nothing from now on.
        let next = u64::from(self.region.generation.wrapping_add(1)) << 2 | FREE;
        slot.state.store(next, Ordering::Release);
    }
}

/// The name a region was registered under, or what it is when it is not registered now: for an
/// error.
fn name_of(region: SpanRegion) -> String {
    let Some(slot) = SLOTS.get(region.slot as usize) else {
        // alloc: an error path.
        return format!("region {} (no slot of the registry)", region.slot);
    };
    if slot.state.load(Ordering::Acquire) != live(region.generation) {
        // alloc: an error path.
        return format!(
            "region {}.{} (not registered now: given back, or never registered)",
            region.slot, region.generation
        );
    }
    let (at, len) = (slot.name.load(Ordering::Relaxed), slot.name_len.load(Ordering::Relaxed));
    // A seqlock's second look: a registration of the slot since would have changed the state
    // before it wrote the name, so an unchanged state says both halves are this region's.
    std::sync::atomic::fence(Ordering::Acquire);
    if slot.state.load(Ordering::Relaxed) != live(region.generation) {
        // alloc: an error path.
        return format!("region {}.{} (given back as it was named)", region.slot, region.generation);
    }
    // SAFETY: `register` stored a `&'static str`'s address and length before it published the
    // slot live with a release store, which the acquire load above saw; a `'static` string lives
    // as long as the program and is UTF-8.
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(at as *const u8, len)) };
    // alloc: an error path.
    format!("{name} (region {})", region.slot)
}

/// A span as a driver reaches it: the address of its first byte and its length, checked to lie
/// inside its registered region.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: a resolved span is an address and a length the registrant promised are its memory's,
// read and written by whichever thread runs the effect (`Spans::register`'s contract).
unsafe impl Send for Resolved {}

impl Resolved {
    pub(crate) fn addr(&self) -> u64 {
        self.at.as_ptr() as u64
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// `f` of the span's bytes, to write into: a read into it.
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        // SAFETY: `Spans::register`'s contract: the bytes are the registrant's, alive while it is
        // registered (checked as the span was resolved), and nothing else touches a read's span
        // while it is in flight, so this is the one reference to them for the call.
        f(unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) })
    }

    /// `f` of the span's bytes, to read: a write from it.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        // SAFETY: as in `with_mut`: nothing writes a write's span while it is in flight.
        f(unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) })
    }
}

/// `span` of the effect `what` of `file` at `at`, through `cache`, as a driver reaches it, or
/// why it is refused: its region not registered now, the span past the region's end, or, to
/// bypass the page cache, an offset, address or length that is not a whole number of
/// [`DIRECT_ALIGN`]. Every driver asks this before anything else, so each refuses the same.
pub(crate) fn resolve(
    span: &Span,
    file: &FileName,
    at: u64,
    cache: Cache,
    what: &str,
) -> Result<Resolved, IoError> {
    let Some(slot) = SLOTS.get(span.region.slot as usize) else {
        return Err(refusal(span, file, at, what, format_args!("no such region")));
    };
    if slot.state.load(Ordering::Acquire) != live(span.region.generation) {
        return Err(refusal(span, file, at, what, format_args!("its region is not registered")));
    }
    let (base, len) = (slot.base.load(Ordering::Relaxed), slot.len.load(Ordering::Relaxed));
    if span.end() > len as u64 {
        return Err(refusal(span, file, at, what, format_args!("past the region's end at {len}")));
    }
    if span.len == 0 {
        return Err(refusal(span, file, at, what, format_args!("an empty span")));
    }
    let start = base + span.offset as usize;
    if cache == Cache::Bypassed {
        let align = DIRECT_ALIGN;
        if at % align != 0 || start as u64 % align != 0 || u64::from(span.len) % align != 0 {
            return Err(refusal(span, file, at, what, format_args!(
                "bypassing the page cache takes an offset, address and length that are each a \
                 whole number of {align} bytes (blaze_core::spans::DIRECT_ALIGN); nothing goes \
                 through the cache instead"
            )));
        }
    }
    match NonNull::new(start as *mut u8) {
        Some(at) => Ok(Resolved { at, len: span.len as usize }),
        None => Err(refusal(span, file, at, what, format_args!("a region at address 0"))),
    }
}

/// Why [`resolve`] refused `span` of the effect `what` of `file` at `at`.
fn refusal(span: &Span, file: &FileName, at: u64, what: &str, why: fmt::Arguments<'_>) -> IoError {
    // alloc: an error path.
    IoError(format!("{what} {file} at {at}, span {span}: {why}"))
}

/// Memory of its own, zeroed and aligned to [`DIRECT_ALIGN`], registered for reads into and
/// writes from it: what a test or a simulation hands [`crate::Effect::ReadInto`] and
/// [`crate::Effect::WriteFrom`]. The store's pool registers its own reserved region instead.
///
/// Its bytes are reached by copies only ([`Memory::read`], [`Memory::write`], [`Memory::fill`]),
/// each a borrow that ends with the call, never a reference held while a driver writes.
pub struct Memory {
    at: NonNull<u8>,
    len: usize,
    registered: Option<Registered>,
}

impl Memory {
    /// `len` bytes (at least one), zeroed, registered as `name`.
    ///
    /// # Safety
    ///
    /// [`Spans::register`]'s contract for every effect asked on its spans: none of [`Memory`]'s
    /// methods is called on bytes of a span while a read into it is in flight, nor
    /// [`Memory::write`] or [`Memory::fill`] on bytes of a span while a write from it is. Under
    /// [`crate::sim::Sim`], which touches the memory only inside its `poll` and `crash` on the
    /// calling thread, no call is a data race; one that breaks the contract reads what the
    /// simulation scribbled, which is what a test of the asker's contract looks for.
    pub unsafe fn new(name: &'static str, len: usize) -> Result<Memory, SpansFull> {
        let layout = layout(len);
        // SAFETY: the layout's size is at least one byte (`layout`).
        // alloc: a test's or a simulation's memory, once per region.
        let at = NonNull::new(unsafe { alloc_zeroed(layout) })
            .unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        // SAFETY: the bytes were just allocated, are this `Memory`'s until it drops, and it drops
        // the registration before it frees them; the caller keeps the contract above.
        match unsafe { Spans::register(name, at, len) } {
            Ok(registered) => Ok(Memory { at, len, registered: Some(registered) }),
            Err(full) => {
                // SAFETY: allocated above with this layout, and registered nowhere.
                unsafe { dealloc(at.as_ptr(), layout) };
                Err(full)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn region(&self) -> SpanRegion {
        self.registered.as_ref().expect("registered until dropped").region()
    }

    /// `len` bytes at `offset`.
    pub fn span(&self, offset: u64, len: u32) -> Span {
        Span { region: self.region(), offset, len }
    }

    /// The bytes `offset..offset + into.len()` copied into `into`.
    pub fn read(&self, offset: usize, into: &mut [u8]) {
        assert!(offset + into.len() <= self.len, "a read past the memory's end");
        // SAFETY: inside the allocation (asserted), and by `Memory::new`'s contract no driver
        // writes these bytes during the call; the slice ends with it.
        let bytes = unsafe { std::slice::from_raw_parts(self.at.as_ptr().add(offset), into.len()) };
        into.copy_from_slice(bytes);
    }

    /// `bytes` copied in at `offset`.
    pub fn write(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.len, "a write past the memory's end");
        // SAFETY: as in `read`, and `&mut self` is the one borrow of the memory here.
        let into =
            unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr().add(offset), bytes.len()) };
        into.copy_from_slice(bytes);
    }

    /// `len` bytes at `offset` set to `byte`: memory given back to the system and not to be read
    /// (the pool's DST poisons evicted pages with `0xDB`).
    pub fn fill(&mut self, offset: usize, len: usize, byte: u8) {
        assert!(offset + len <= self.len, "a fill past the memory's end");
        // SAFETY: as in `write`.
        let into = unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr().add(offset), len) };
        into.fill(byte);
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // Unregistered before the bytes go, so no span of it resolves to freed memory.
        drop(self.registered.take());
        // SAFETY: allocated in `new` with this layout, and no longer registered.
        unsafe { dealloc(self.at.as_ptr(), layout(self.len)) };
    }
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Memory({} bytes, {:?})", self.len, self.registered)
    }
}

fn layout(len: usize) -> Layout {
    Layout::from_size_align(len.max(1), DIRECT_ALIGN as usize).expect("a memory's size fits")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    /// A span resolves only while its region is registered and only inside it; one given back
    /// is refused by its generation even once its slot holds another region; bypassing the cache
    /// takes whole blocks of offset, address and length.
    #[test]
    fn a_span_resolves_only_inside_a_live_region() {
        let file = name("pool/pack-1");
        // SAFETY: no effect is asked on its spans.
        let memory = unsafe { Memory::new("test memory", 3 * DIRECT_ALIGN as usize) }.unwrap();
        let span = memory.span(DIRECT_ALIGN, DIRECT_ALIGN as u32);
        let resolved = resolve(&span, &file, 0, Cache::Kept, "read into").unwrap();
        assert_eq!(resolved.len(), DIRECT_ALIGN as usize);
        assert!(resolve(&span, &file, 0, Cache::Bypassed, "read into").is_ok());
        let odd = memory.span(1, DIRECT_ALIGN as u32);
        assert!(resolve(&odd, &file, 0, Cache::Kept, "read into").is_ok());
        let why = resolve(&odd, &file, 0, Cache::Bypassed, "read into").unwrap_err();
        assert!(why.0.contains("DIRECT_ALIGN") && why.0.contains("pool/pack-1"), "{why}");
        assert!(resolve(&span, &file, 1, Cache::Bypassed, "read into").is_err());
        let past = memory.span(2 * DIRECT_ALIGN, DIRECT_ALIGN as u32 + 1);
        let why = resolve(&past, &file, 0, Cache::Kept, "read into").unwrap_err();
        assert!(why.0.contains("past the region's end") && why.0.contains("test memory"), "{why}");
        drop(memory);
        let why = resolve(&span, &file, 0, Cache::Kept, "read into").unwrap_err();
        assert!(why.0.contains("not registered"), "{why}");
        // SAFETY: as above.
        let again = unsafe { Memory::new("another", DIRECT_ALIGN as usize * 4) }.unwrap();
        assert!(resolve(&span, &file, 0, Cache::Kept, "read into").is_err());
        drop(again);
    }

    /// A slot given back is taken again, so registering and dropping more regions than the
    /// table has slots never fills it.
    #[test]
    fn regions_given_back_free_their_slots() {
        for _ in 0..3 * REGIONS {
            // SAFETY: no effect is asked on its spans.
            let memory = unsafe { Memory::new("churn", 16) }.unwrap();
            drop(memory);
        }
    }
}
