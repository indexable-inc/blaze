//! Per-worker write segments (docs/v2/plan/design/concurrency.md "Writes").
//!
//! A worker puts through a [`Writer`] of its own: it encodes the value, writes the record into
//! its own segment's memory and publishes the record's key into the shared delta table with
//! one compare-and-swap. No lock, and no shared write but that slot. The store drains every
//! segment when a commit begins: it copies what each published into its packs, and once those
//! are mapped it points each key at its pack with one swap, after which the segment's memory
//! is freed at task boundaries.
//!
//! A segment is a list of chunks, each written by its one writer past what it has published
//! and read by everyone below it. A record in a chunk is a 16-byte frame (its unit, whether it
//! is a copy) then the record as a pack holds it, except that a value that is a file of its
//! own carries its bytes until the store writes the file.

use std::{
    ptr::{self, NonNull},
    sync::Arc,
};

use blaze_ids::ContentId;
use blaze_mem::{MemError, Region, Tag, scratch};
use blaze_reclaim::Guard;

use crate::sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, Ordering},
    track,
};
use crate::{
    CHUNK_FROM, EXTERNAL_FROM, Key, Kind, Mismatch, Ticket, Unit, chunk, cold, cold_columns,
    disk::{self, CHUNK_DOMAIN, Form, NO_DICTIONARY, RECORD_HEADER, RecordHeader, content_of},
    flags::Flag,
    read::{Look, Place, Seg, Shared, Spot, carry_flags},
    table::{Held, Table},
    words::stop,
};

/// A chunk's length, unless a record needs more.
#[cfg(not(test))]
pub(crate) const CHUNK: usize = 1 << 20;
#[cfg(test)]
pub(crate) const CHUNK: usize = 8 << 10;

/// What a record in a segment starts with: its unit (4 bytes), whether it is a copy (1 byte),
/// zeros to 16.
pub(crate) const FRAME: u64 = 16;

/// A frame's copy byte for a record no key points at: a chunk of an output another writer has
/// put and not yet made durable, written again so this writer's put does not wait on that
/// one's. A chunk's key is the hash of its bytes, so the two are the same value.
pub(crate) const COPY: u8 = 1;

/// A chunk's published word: bytes below `1 << BYTES_BITS`, puts ended in it above.
const BYTES_BITS: u32 = 40;

/// Puts that may end in one chunk: a put that would reach it rolls to a new chunk before it is
/// counted ([`Own::room_for_a_put`]), so [`Chunk::publish`] never refuses one.
const MOST_PUTS: u64 = 1 << (64 - BYTES_BITS);

/// Bytes one chunk may hold: a record whose span with its frame would reach it is refused
/// before it is written ([`Own::write`]), so every chunk's cap, and what it publishes, stays
/// below it and [`Chunk::publish`] never refuses one.
const MOST_BYTES: u64 = 1 << BYTES_BITS;

/// Where a chunk's bytes start in its region: its header comes first, a cache line to itself.
const HEAD: usize = std::mem::size_of::<Chunk>().next_multiple_of(64);

/// A chunk lives at the base of a region of its own, tagged [`Tag::Store`] and committed
/// whole when it is made (the kernel's zeroed pages), its bytes after it: making one calls no
/// allocator and freeing it hands the pages back.
pub(crate) struct Chunk {
    region: Region,
    bytes: NonNull<u8>,
    cap: usize,
    published: AtomicU64,
    next: AtomicPtr<Chunk>,
    /// Where it starts among every byte its segment has written: a place is unique by it.
    pub(crate) start: u64,
}

// SAFETY: a chunk's bytes are written only by its segment's one writer, at or past what it has
// kept; others read only bytes a published place names or that were published (both with
// release after the bytes were written), and those are never written again. The bytes are
// freed only with the chunk's region.
unsafe impl Send for Chunk {}
// SAFETY: as for `Send`: through `&Chunk` other threads read only bytes a published place names
// or that were published, never written again, and the atomics; the writer writes only through
// `unsafe fn write` and `zero`, whose callers write past what was kept.
unsafe impl Sync for Chunk {}

impl Chunk {
    fn new(cap: usize, start: u64) -> ChunkRef {
        let whole = cap.checked_add(HEAD).unwrap_or_else(|| {
            stop(&MemError::TooLarge { tag: Tag::Store, what: "a segment chunk", wanted: cap })
        });
        let region = match Region::reserve(whole, Tag::Store) {
            Ok(region) => region,
            Err(error) => stop(&error),
        };
        if let Err(error) = region.ensure(whole) {
            stop(&error)
        }
        let at = region.base().cast::<Chunk>();
        let bytes = region.at(HEAD);
        track::write(bytes.as_ptr(), cap);
        let chunk = Chunk {
            region,
            bytes,
            cap,
            published: AtomicU64::new(0),
            next: AtomicPtr::new(ptr::null_mut()),
            start,
        };
        // SAFETY: the region's base is page-aligned and its first `HEAD` bytes, at least a
        // `Chunk`, are committed (ensured above) and hold nothing yet. The chunk now owns the
        // region it sits in, freed only by `ChunkRef::free`.
        unsafe { at.write(chunk) };
        ChunkRef(at)
    }

    pub(crate) fn cap(&self) -> u64 {
        self.cap as u64
    }

    /// The bytes at `at`; `None` past the chunk.
    ///
    /// # Safety
    ///
    /// The range is one a published place names, or below what the chunk published: the
    /// writer may be writing any other bytes of it.
    pub(crate) unsafe fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end > self.cap as u64 {
            return None;
        }
        // SAFETY: `at + len <= cap` (checked above), so `at` is inside the `cap` committed bytes
        // after the header.
        let start = unsafe { self.bytes.as_ptr().add(at as usize) };
        track::read(start, len as usize);
        // SAFETY: the `len` bytes from `start` are committed bytes of the region, which lives as
        // long as the chunk; bytes a place names or that were published are not written again
        // (the caller's contract).
        Some(unsafe { std::slice::from_raw_parts(start, len as usize) })
    }

    /// Writes `data` at `at`.
    ///
    /// # Safety
    ///
    /// Only the segment's writer calls it, at or past what it has kept, where no place points
    /// and nothing was published.
    unsafe fn write(&self, at: usize, data: &[u8]) {
        assert!(at + data.len() <= self.cap, "a write past its chunk");
        let from = data.as_ptr();
        // SAFETY: `at + data.len() <= cap` (asserted above), so `at` is inside the bytes.
        let to = unsafe { self.bytes.as_ptr().add(at) };
        track::write(to, data.len());
        // SAFETY: `to` has `data.len()` writable committed bytes, which `data` (the
        // caller's own buffer) does not overlap, and no one else reads or writes them (the
        // caller's contract).
        unsafe { ptr::copy_nonoverlapping(from, to, data.len()) }
    }

    /// Writes `len` zeros at `at`.
    ///
    /// # Safety
    ///
    /// As `write`.
    unsafe fn zero(&self, at: usize, len: usize) {
        assert!(at + len <= self.cap, "a write past its chunk");
        // SAFETY: `at + len <= cap` (asserted above), so `at` is inside the bytes.
        let to = unsafe { self.bytes.as_ptr().add(at) };
        track::write(to, len);
        // SAFETY: `to` has `len` writable committed bytes, and no one else reads or
        // writes them (the caller's contract, as in `write`).
        unsafe { ptr::write_bytes(to, 0, len) }
    }

    /// Bytes of it its writer has published, and how many puts ended in it.
    pub(crate) fn published(&self) -> (u64, u64) {
        let word = self.published.load(Ordering::Acquire);
        (word & ((1 << BYTES_BITS) - 1), word >> BYTES_BITS)
    }

    fn publish(&self, bytes: u64, puts: u64) {
        assert!(bytes < MOST_BYTES && puts < MOST_PUTS, "a chunk of 2^40 bytes or 2^24 puts");
        self.published.store(bytes | (puts << BYTES_BITS), Ordering::Release);
    }

    /// The chunk after it, once its writer has moved on: then what it published is all of it.
    pub(crate) fn next(&self) -> Option<ChunkRef> {
        NonNull::new(self.next.load(Ordering::Acquire)).map(ChunkRef)
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        // The region, unmapped once this returns, takes the bytes with it.
        track::free(self.bytes.as_ptr(), self.cap);
    }
}

/// A chunk of a segment, by address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ChunkRef(NonNull<Chunk>);

// SAFETY: a chunk is `Sync`; a reference is dereferenced only while its chunk is alive (see
// `ChunkRef::get`).
unsafe impl Send for ChunkRef {}
// SAFETY: a `ChunkRef` is an address: sharing it shares nothing until `ChunkRef::get`, whose
// caller promises the chunk is alive, and `Chunk` is `Sync`.
unsafe impl Sync for ChunkRef {}

impl ChunkRef {
    /// The chunk it names.
    ///
    /// # Safety
    ///
    /// The chunk must live for `'a`: it is the writer's tail, or not yet unlinked by the store,
    /// or named by a spot the caller found under a `Held` of `'a` (a chunk is freed only
    /// through the store's limbo, after every spot into it was unlinked).
    pub(crate) unsafe fn get<'a>(self) -> &'a Chunk {
        // SAFETY: alive, by the caller's contract.
        unsafe { self.0.as_ref() }
    }

    /// Frees the chunk and the region it sits in.
    ///
    /// # Safety
    ///
    /// The caller owns the chunk (unlinked, or its segment being dropped), and nothing reads it
    /// any more nor frees it again.
    unsafe fn free(self) {
        // SAFETY: the chunk was written at its region's base by `Chunk::new` and is read out
        // once, by its one owner: the copy owns the region, so dropping it unmaps the original.
        drop(unsafe { self.0.read() });
    }
}

/// A chunk unlinked from its segment, freed when this drops: what the store holds in its limbo
/// until no reader can be in the chunk.
pub(crate) struct Unlinked(ChunkRef);

impl Drop for Unlinked {
    fn drop(&mut self) {
        // SAFETY: `Segment::unlink_first` made it the chunk's one owner, and the store drops it
        // only once a grace begun after the unlink has passed.
        unsafe { self.0.free() }
    }
}

/// One writer's segment, as the store and the writer share it.
pub(crate) struct Segment {
    pub(crate) number: u32,
    /// The oldest chunk not freed: the store moves it on as it frees chunks.
    first: AtomicPtr<Chunk>,
    /// How many of its puts are durable.
    durable: AtomicU64,
    /// How many puts its writer has begun past finding the key kept already: a put is counted
    /// here before any key of it is readable (before its first claim), so a reader that saw
    /// one of its keys and then reads this sees the put counted. What a wait for every put made
    /// so far waits on ([`crate::Store::last_puts`]). A put counted here may not be published
    /// to the store's drain yet ([`Own::end_put`] publishes, then counts it again): a wait on
    /// it flushes again until a commit takes it.
    made: AtomicU64,
    /// Why a put of it could not be kept: zstd refused the value. The store stops on it.
    failed: OnceLock<String>,
}

impl Segment {
    pub(crate) fn first(&self) -> ChunkRef {
        ChunkRef(NonNull::new(self.first.load(Ordering::Acquire)).expect("a segment has a chunk"))
    }

    pub(crate) fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    pub(crate) fn set_durable(&self, puts: u64) {
        self.durable.store(puts, Ordering::Release);
    }

    /// The ticket of its writer's last put, if that is not durable yet: once it is, every put
    /// of the segment before it is too (a segment's puts turn durable in order).
    pub(crate) fn last_pending(&self) -> Option<crate::Ticket> {
        let made = self.made.load(Ordering::Acquire);
        (made > self.durable()).then(|| crate::Ticket::of(self.number, made))
    }

    pub(crate) fn failed(&self) -> Option<&str> {
        self.failed.get().map(String::as_str)
    }

    /// Unlinks its oldest chunk, `chunk`, which its writer has moved on from and no place names
    /// any more, for the caller to hold in its limbo. Only the store calls it.
    pub(crate) fn unlink_first(&self, chunk: ChunkRef) -> Unlinked {
        assert_eq!(self.first(), chunk, "a segment's chunks are freed oldest first");
        // SAFETY: the first chunk, which only this call frees.
        let next = unsafe { chunk.get() }.next().expect("the chunk a writer writes is not freed");
        self.first.store(next.0.as_ptr(), Ordering::Release);
        // Unlinked just now, so owned by no one else: the segment's drop starts at `first`.
        Unlinked(chunk)
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let mut at = NonNull::new(*self.first.get_mut()).map(ChunkRef);
        while let Some(chunk) = at {
            // SAFETY: the chunks from `first` on are this segment's, and nothing reads a segment
            // being dropped: its writer and the store let go of it.
            at = unsafe { chunk.get() }.next();
            // SAFETY: as above; its next was read just before.
            unsafe { chunk.free() };
        }
    }
}

/// A worker's way to put values into the store: one a worker, from [`crate::Store::writer`],
/// used from a task of any thread.
///
/// A put is readable by every reader at once and durable once the store has committed it:
/// [`Writer::durable`] says when.
pub struct Writer {
    pub(crate) shared: Arc<Shared>,
    pub(crate) own: Own,
}

impl Writer {
    /// [`Writer::put_in`] of [`Unit::SHARED`].
    pub fn put(
        &mut self,
        guard: &Guard,
        key: Key,
        kind: Kind,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        self.put_in(guard, Unit::SHARED, key, kind, bytes)
    }

    /// Keeps `bytes` under `key` in the packs of `unit`, as [`crate::Store::put_in`] does.
    pub fn put_in(
        &mut self,
        guard: &Guard,
        unit: Unit,
        key: Key,
        kind: Kind,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        let content = content_of(bytes);
        self.own.put(Look::new(&self.shared, Held::task(guard)), unit, key, kind, content, bytes)
    }

    /// Whether the put that got `ticket` (from any writer of the store) is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }

    /// A put counted ([`crate::Store::last_puts`] covers it) that writes nothing and ends when
    /// the [`HeldPut`] drops: what another thread's put is to a waiter between its count and
    /// its publish, for as long as a test holds it.
    #[cfg(feature = "test-hooks")]
    pub fn hold_put(&mut self) -> HeldPut<'_> {
        self.own.room_for_a_put();
        self.own.segment.made.store(self.own.puts + 1, Ordering::Release);
        HeldPut(&mut self.own)
    }
}

/// A put counted and not yet ended, from [`Writer::hold_put`]: it ends, empty, when dropped.
#[cfg(feature = "test-hooks")]
pub struct HeldPut<'a>(&'a mut Own);

#[cfg(feature = "test-hooks")]
impl Drop for HeldPut<'_> {
    fn drop(&mut self) {
        self.0.end_put();
    }
}

/// The writing end of a segment.
pub(crate) struct Own {
    segment: Arc<Segment>,
    tail: ChunkRef,
    /// Bytes of `tail` kept: published, or of the put being made.
    at: u64,
    /// Puts begun.
    puts: u64,
    /// Puts that ended in `tail`.
    puts_here: u64,
    /// Run by each put at each [`PutAt`]: a test holds a put there, counted and not
    /// published, while another thread reads, puts or flushes.
    #[cfg(test)]
    pub(crate) pause: Option<Arc<dyn Fn(PutAt) + Send + Sync>>,
}

/// Where a put is when it runs a test's [`Own::pause`].
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PutAt {
    /// Counted in `made`, before it wrote or claimed anything.
    Counted,
    /// An output's chunk looked up and not found kept, before its claim: another writer may
    /// claim that chunk first.
    ChunkMissed,
    /// A record written and the view it claims the key in loaded, before the key goes into
    /// that view's tables (`prev` first, then the delta): an index swap may come between,
    /// leaving that delta as `prev`, or an `end_migration`, leaving that `prev` as no store's.
    Claiming,
    /// The key won in the view's `prev`, before it goes into the delta: another writer's claim
    /// may meet it in `prev` while the delta lacks it.
    InPrev,
    /// The key lost in the view's `prev`, before the claim reads the delta: a move may take
    /// `prev`'s spot it lost to meanwhile.
    LostInPrev,
    /// Its records written and its keys claimed (readable), before it ends.
    Written,
}

/// A put counted in its segment's `made`: it ends ([`Own::end_put`]) when this drops, a panic
/// of the put's included, so `made` is never left ahead of what its writer publishes (a
/// `Writer` lives on past a panic of its put: its lock does not poison). Every record the put
/// wrote is whole before its key is published, so publishing them is sound; a put that wrote
/// nothing ends empty, and the next flush with nothing to write makes its count durable.
struct Counted<'a>(&'a mut Own);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.end_put();
    }
}

impl Own {
    pub(crate) fn new(number: u32) -> Own {
        let tail = Chunk::new(CHUNK, 0);
        // alloc: one a writer, shared with the store for the writer's life
        let segment = Arc::new(Segment {
            number,
            first: AtomicPtr::new(tail.0.as_ptr()),
            durable: AtomicU64::new(0),
            made: AtomicU64::new(0),
            failed: OnceLock::new(),
        });
        Own {
            segment,
            tail,
            at: 0,
            puts: 0,
            puts_here: 0,
            #[cfg(test)]
            pause: None,
        }
    }

    pub(crate) fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// Keeps `bytes`, whose hash is `content`, under `key`.
    pub(crate) fn put(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        key: Key,
        kind: Kind,
        content: ContentId,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        if let Some((kept, ticket)) = look.kept(&key) {
            return match kept == content {
                true => Ok(ticket),
                false => Err(Mismatch { key, kept, given: content }),
            };
        }
        self.room_for_a_put();
        let ticket = Ticket::of(self.segment.number, self.puts + 1);
        // Counted before any key of it is readable: a reader on another thread that reads one
        // of its keys from here on, then waits for every put made, waits for this one.
        self.segment.made.store(self.puts + 1, Ordering::Release);
        let counted = Counted(self);
        #[cfg(test)]
        if let Some(pause) = &counted.0.pause {
            pause(PutAt::Counted);
        }
        counted.0.make(look, unit, key, kind, content, bytes, ticket)
    }

    /// The records of a put counted as `ticket` ([`Own::put`]), which ends it.
    fn make(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        key: Key,
        kind: Kind,
        content: ContentId,
        bytes: &[u8],
        ticket: Ticket,
    ) -> Result<Ticket, Mismatch> {
        let raw = bytes.len() as u64;
        let record = |form, dictionary, stored: &[u8]| RecordHeader {
            kind,
            form,
            dictionary,
            stored: stored.len() as u64,
            raw,
            key,
            content,
        };
        let won = if kind == Kind::Output && bytes.len() >= CHUNK_FROM {
            // The list of chunks and each chunk's LZ4 are made in the thread's reused buffers.
            scratch::with_bytes(|payload| {
                scratch::with_bytes(|packed| {
                    for range in chunk::cuts(bytes) {
                        let piece = &bytes[range];
                        let chunk = Key(ContentId::of(CHUNK_DOMAIN, piece));
                        let kept = look.kept(&chunk);
                        let copy = kept.is_some_and(|(_, there)| self.waits_on(look, there));
                        #[cfg(test)]
                        if kept.is_none()
                            && let Some(pause) = &self.pause
                        {
                            pause(PutAt::ChunkMissed);
                        }
                        if kept.is_none() || copy {
                            let (form, stored) = match cold::lz4_into(piece, packed) {
                                true => (Form::Lz4, &packed[..]),
                                false => (Form::Raw, piece),
                            };
                            let header = RecordHeader {
                                kind: Kind::Output,
                                form,
                                dictionary: 0,
                                stored: stored.len() as u64,
                                raw: piece.len() as u64,
                                key: chunk,
                                content: content_of(piece),
                            };
                            // A chunk another put claimed between the look and this claim is the
                            // same bytes (its key is their hash), but its record may be that
                            // put's, not yet published: then this put keeps a copy, as it does
                            // for one found kept at the look, or its ticket could turn durable
                            // with the chunk only in the other writer's memory.
                            let lost = match copy {
                                true => true,
                                false => self
                                    .claim(look, Unit::SHARED, header, stored, ticket)
                                    .is_err_and(|(_, there)| self.waits_on(look, there)),
                            };
                            if lost {
                                self.write_copy(Unit::SHARED, &header, stored);
                            }
                        }
                        disk::encode_chunk(payload, chunk, piece.len() as u64);
                    }
                });
                let payload = &payload[..];
                self.claim(look, unit, record(Form::Chunked, 0, payload), payload, ticket)
            })
        } else if (kind == Kind::Machine && !bytes.is_empty())
            || (!kind.cold() && kind != Kind::Output && bytes.len() >= EXTERNAL_FROM)
        {
            // Its bytes ride in the segment; the store writes them as the file at the commit.
            self.claim(look, unit, record(Form::External, 0, bytes), bytes, ticket)
        } else if let Some(columns) = look.columns_of(bytes).filter(|_| kind.cold()) {
            match cold_columns(columns, bytes) {
                Ok(payload) => {
                    self.claim(look, unit, record(Form::ColdColumns, 0, &payload), &payload, ticket)
                }
                Err(why) => return Ok(self.fail(kind, &why)),
            }
        } else if kind.cold() && bytes.len() >= cold::LONG_FROM {
            match cold::compress_long(bytes) {
                Ok(stored) => self.claim(
                    look,
                    unit,
                    record(Form::Zstd, NO_DICTIONARY, &stored),
                    &stored,
                    ticket,
                ),
                Err(why) => return Ok(self.fail(kind, &why)),
            }
        } else if kind.cold()
            && let Some((number, dictionary)) = look.dictionary_of(kind)
        {
            match cold::compress(dictionary, bytes) {
                Ok(stored) => {
                    self.claim(look, unit, record(Form::Zstd, number, &stored), &stored, ticket)
                }
                Err(why) => return Ok(self.fail(kind, &why)),
            }
        } else {
            // A cold kind with no dictionary yet is kept raw and sampled, and its dictionary
            // trained on the samples.
            self.claim(look, unit, record(Form::Raw, 0, bytes), bytes, ticket)
        };
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause(PutAt::Written);
        }
        match won {
            Ok(()) => Ok(ticket),
            Err((kept, there)) if kept == content => Ok(there),
            Err((kept, _)) => Err(Mismatch { key, kept, given: content }),
        }
    }

    /// zstd refusing a value is a broken build of it (a missing feature), not something a
    /// value can cause: the store stops on it rather than keep the value another way.
    fn fail(&mut self, kind: Kind, why: &str) -> Ticket {
        // alloc: the store's failure, once: it stops on it
        self.stop(format!("compressing a {} value: {why}", kind.name()))
    }

    /// Stops the store on `why`, a put this segment cannot keep. The put still takes its
    /// ticket, which is never durable, and ends as it returns ([`Counted`]), empty.
    fn stop(&mut self, why: String) -> Ticket {
        _ = self.segment.failed.set(why);
        Ticket::of(self.segment.number, self.puts + 1)
    }

    /// Whether a put of this segment that found `there`'s record under a key would wait on
    /// another segment's put not yet durable: then it writes the record again ([`COPY`]).
    fn waits_on(&self, look: Look<'_>, there: Ticket) -> bool {
        there.segment() != self.segment.number && !look.shared.durable(there)
    }

    /// Writes a record and publishes its key; when the key was published meanwhile, keeps
    /// nothing and gives back the content and ticket of what is there. A record [`Own::write`]
    /// refuses keeps nothing either, and gives `Ok`: the store stops on it.
    ///
    /// During an index swap the view has two tables, `prev` (the delta the swap left) and the
    /// new delta, and the key goes into `prev` first. A writer that loaded the view before the
    /// swap claims with `prev` as its last table (after an older `prev`, for a view of the swap
    /// before), so every claim of a key while `prev` is the store's meets the others at one
    /// slot, `prev`'s, which one compare-and-swap wins. The new delta then holds a key `prev`
    /// holds only at the same place: the swap copies `prev`'s spots in, a claim adds the one it
    /// won in `prev`, a claim that lost in `prev` copies the spot it lost to
    /// (`Own::lost_in_prev`), and a move of `prev`'s spot puts its pack spot into the delta as
    /// well (`Store::finish_batches`), where it meets such a copy at one slot. So no writer
    /// claims in the delta a key whose record `prev` already holds (made durable, perhaps, and
    /// dropped by `end_migration` in favour of a record not yet durable), and a second value
    /// put across a swap is refused (a [`Mismatch`]) rather than kept beside the first.
    ///
    /// The delta can refuse a key won in `prev` only once `end_migration` has run and the view
    /// this claim loaded is stale: then `prev` is no store's table, and the delta decides. The
    /// record stays kept (a stale reader may find it in `prev`): of the same value, the drain
    /// makes it the home of the other record or a duplicate (`Store::record_home`), as for a
    /// put whose look missed; of another value, the put is refused.
    fn claim(
        &mut self,
        look: Look<'_>,
        unit: Unit,
        header: RecordHeader,
        payload: &[u8],
        ticket: Ticket,
    ) -> Result<(), (ContentId, Ticket)> {
        // Refused by its length: the store stops on it, and the put ends having kept nothing,
        // its ticket never durable.
        let Some((seg, span)) = self.write(unit, &header, payload, 0) else { return Ok(()) };
        let tier = &look.view().tiers[0];
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause(PutAt::Claiming);
        }
        let mut kept = false;
        #[cfg(test)]
        let mut paused = false;
        for table in tier.prev.iter().chain([&tier.delta]) {
            let spot = Spot::new(header.key, header.content, Place::Segment(seg), ticket);
            match table.insert(look.held, spot) {
                Ok((row, _)) => {
                    if !kept {
                        // Kept before anything else can panic: the key now names these bytes,
                        // so the writer's next record must not be written over them.
                        self.at += span;
                        kept = true;
                    }
                    table.set_flag(row, Flag::New as usize);
                    // The first table won is `prev` whenever the view has one.
                    #[cfg(test)]
                    if let Some(pause) = &self.pause
                        && tier.prev.is_some()
                        && !std::mem::replace(&mut paused, true)
                    {
                        pause(PutAt::InPrev);
                    }
                }
                // Lost in the delta, or in a `prev` no longer the store's, which the delta
                // decides (`Own::lost_in_prev`).
                Err((_, (_, there))) if !kept && tier.prev.is_none() => {
                    return Err((there.content, there.ticket));
                }
                Err((_, (row, there))) if !kept => {
                    let prev = tier.prev.as_deref().expect("a view with `prev`, the arm above");
                    return Err(self.lost_in_prev(look, prev, row, there, &tier.delta));
                }
                // Won in `prev`, and the delta holds this spot already: a claim that lost in
                // `prev` put it there (`Own::lost_in_prev`), or `end_migration` copied it. It is
                // this put's record.
                Err((_, (row, there))) if there.place == Place::Segment(seg) => {
                    table.set_flag(row, Flag::New as usize);
                }
                // Won in `prev`, and the delta holds another record of the same value. With a
                // stale view, another writer's, which the drain sorts (`Store::record_home`).
                // With a fresh one, the pack spot of a chunk's copy (`COPY`): its move took
                // this claim's spot in `prev` while this claim was between its inserts
                // (`Store::copy_home`) and put the pack spot into the delta too, so this
                // record drains as a duplicate.
                Err((_, (_, there))) if there.content == header.content => {}
                Err((_, (_, there))) => return Err((there.content, there.ticket)),
            }
        }
        Ok(())
    }

    /// What a claim that lost `prev`'s slot to `there` (at `row`) meets: the delta's spot of the
    /// key.
    ///
    /// While `prev` is the store's, the delta holds the key only at the place `prev` holds it
    /// (see [`Own::claim`]), so its answer is `prev`'s. Once `end_migration` has run, `prev` is
    /// no store's table and `there` may be a put the delta refused: a writer that won the dead
    /// `prev` and met another value in the delta, its put a [`Mismatch`] and its ticket no key's
    /// home. Taking that ticket would end this put `Ok` on a record the key never reads (review
    /// 12, L1), so the delta decides.
    ///
    /// The delta can lack the key, at a segment place: while the claim that won `prev`'s slot
    /// is between its two inserts, or after a claim from a view older than the swap, whose
    /// delta `prev` was, put it there alone. This claim then puts `there` into the delta, so the
    /// delta's answer is fixed before this put ends: a writer with a fresh view can no longer
    /// win the delta after this put took `there`'s ticket, and a claim still between its
    /// inserts meets its own place there and is kept. `there` is what `prev` held when this
    /// claim lost, and a move may have taken it since: of the record of a claim that ended, or
    /// of a chunk's copy standing in for a record whose claim is still between its inserts
    /// (`Store::copy_home`). A move of `prev`'s spot during a swap also puts its pack spot into
    /// the delta
    /// (`Store::finish_batches`), so the two meet at the delta's one slot: a copy put first is
    /// replaced by the move, and a copy put after is refused and answers with the pack spot.
    ///
    /// At a pack place, `there` was moved: the record of a claim that ended, or a chunk's copy
    /// standing in for a record whose claim may still be between its inserts. The delta holds
    /// it (found here: a move during the swap put it there too), or it was moved before the
    /// swap and the fold will copy it, or the index holds it, which the swap and the fold leave
    /// out. Its content and ticket are the key's, and a move never changes them.
    fn lost_in_prev(
        &self,
        look: Look<'_>,
        prev: &Table<Spot>,
        row: u64,
        there: &Spot,
        delta: &Table<Spot>,
    ) -> (ContentId, Ticket) {
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause(PutAt::LostInPrev);
        }
        if let Some((_, kept)) = delta.get(look.held, &there.key) {
            return (kept.content, kept.ticket);
        }
        if !matches!(there.place, Place::Segment(_)) {
            return (there.content, there.ticket);
        }
        match delta.insert(look.held, there.at(there.place)) {
            Ok((at, _)) => {
                carry_flags(prev, row, delta, at);
                (there.content, there.ticket)
            }
            Err((_, (_, kept))) => (kept.content, kept.ticket),
        }
    }

    /// Writes a record no key will point at, kept for its bytes (see [`COPY`]).
    fn write_copy(&mut self, unit: Unit, header: &RecordHeader, payload: &[u8]) {
        if let Some((_, span)) = self.write(unit, header, payload, COPY) {
            self.at += span;
        }
    }

    /// Writes a record at the writer's next bytes, not yet kept: gives its place and length.
    /// `None` for a record whose span with its frame reaches [`MOST_BYTES`]: it writes nothing
    /// and stops the store on it, naming the record's key ([`Segment::failed`]).
    pub(crate) fn write(
        &mut self,
        unit: Unit,
        header: &RecordHeader,
        payload: &[u8],
        copy: u8,
    ) -> Option<(Seg, u64)> {
        assert_eq!(payload.len() as u64, header.stored, "a record's payload is its stored length");
        let span = FRAME + header.span();
        // Every form's exact length, checked before a chunk is made for it: a chunk's cap is
        // the larger of `CHUNK` and the span it was rolled for, so each stays below the bound.
        if span >= MOST_BYTES {
            // alloc: the store's failure, once: it stops on it
            _ = self.segment.failed.set(format!(
                "segment {}: the {} record of {:x}, {span} bytes with its frame ({:?}, {} raw), \
                 reaches a segment chunk's bound of {MOST_BYTES} bytes",
                self.segment.number,
                header.kind.name(),
                header.key.0,
                header.form,
                header.raw,
            ));
            return None;
        }
        if self.at + span > self.tail().cap() {
            self.roll(span);
        }
        let at = self.at as usize;
        let tail = self.tail();
        let mut frame = [0u8; FRAME as usize];
        frame[..4].copy_from_slice(&unit.0.to_le_bytes());
        frame[4] = copy;
        let body = at + FRAME as usize;
        let end = body + RECORD_HEADER as usize + payload.len();
        // SAFETY: this is the segment's one writer (`&mut self`), and `at` is past what it has
        // kept, so no place points into `at..at + span` and nothing there was published; `roll`
        // left room for `span` bytes.
        unsafe { tail.write(at, &frame) };
        // SAFETY: as above, for the header after the frame.
        unsafe { tail.write(body, &header.encode()) };
        // SAFETY: as above, for the payload after the header.
        unsafe { tail.write(body + RECORD_HEADER as usize, payload) };
        // SAFETY: as above, for the zeros up to `at + span`.
        unsafe { tail.zero(end, at + span as usize - end) };
        let seg = Seg {
            chunk: self.tail,
            segment: self.segment.number,
            offset: self.at + FRAME,
            kind: header.kind,
            form: header.form,
            stored: header.stored,
            raw: header.raw,
        };
        Some((seg, span))
    }

    /// Keeps and publishes the record [`Own::write`] wrote last, `span` bytes: for the store's
    /// own records (moved by a settle), whose keys it points at itself.
    pub(crate) fn keep(&mut self, span: u64) {
        self.at += span;
        self.tail().publish(self.at, self.puts_here);
    }

    fn tail(&self) -> &Chunk {
        // SAFETY: the tail has no next, and the store frees only chunks with a next.
        unsafe { self.tail.get() }
    }

    /// Seals the tail with what it holds and moves on to a chunk with room for `need` bytes.
    fn roll(&mut self, need: u64) {
        let old = self.tail();
        let next = Chunk::new((need as usize).max(CHUNK), old.start + old.cap());
        old.publish(self.at, self.puts_here);
        old.next.store(next.0.as_ptr(), Ordering::Release);
        self.tail = next;
        self.at = 0;
        self.puts_here = 0;
    }

    /// Rolls to a new chunk when one more put would pass what a chunk counts ([`MOST_PUTS`]):
    /// run before a put is counted, so its [`Own::end_put`] (in [`Counted`]'s drop, perhaps in
    /// an unwind) never meets [`Chunk::publish`]'s bound.
    fn room_for_a_put(&mut self) {
        if self.puts_here + 1 >= MOST_PUTS {
            self.roll(0);
        }
    }

    fn end_put(&mut self) {
        let puts_here = self.puts_here + 1;
        // Published before `puts`/`puts_here` move, so they never pass what the chunk
        // published. `made` was stored at the count (`Own::put`), so a panic here would leave
        // it one ahead: what keeps `publish` from refusing are the bounds checked before the
        // put was counted (`room_for_a_put`) and before each record is written (`write`).
        self.tail().publish(self.at, puts_here);
        self.puts += 1;
        self.puts_here = puts_here;
        // After the publish: a load of `made` that sees this count sees the record published,
        // so a commit begun after it drains the put.
        self.segment.made.store(self.puts, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_publishes_its_bytes_and_puts_in_one_word() {
        let chunk = Chunk::new(4096, 0);
        // SAFETY: made just now and freed below.
        let it = unsafe { chunk.get() };
        assert_eq!(it.published(), (0, 0));
        it.publish(4000, 3);
        assert_eq!(it.published(), (4000, 3));
        // SAFETY: made here, no one else holds it.
        unsafe { chunk.free() };
    }

    /// A chunk's bytes start zero, hold what is written past any commit step of its region, and
    /// a chunk bigger than the usual takes the size asked for.
    #[test]
    #[cfg(not(blaze_check))]
    fn a_chunk_reads_back_what_was_written_across_its_pages() {
        let cap = 3 * CHUNK + 5;
        let chunk = Chunk::new(cap, 7);
        // SAFETY: made just now and freed below.
        let it = unsafe { chunk.get() };
        assert_eq!((it.cap(), it.start), (cap as u64, 7));
        // SAFETY: nothing was published or placed: this test is its one writer and reader.
        assert!(unsafe { it.bytes(0, cap as u64) }.unwrap().iter().all(|it| *it == 0));
        let mut model = vec![0u8; cap];
        for (n, at) in (0..cap).step_by(4093).enumerate() {
            let data = [n as u8 | 1; 9];
            let len = data.len().min(cap - at);
            // SAFETY: as above.
            unsafe { it.write(at, &data[..len]) };
            model[at..at + len].copy_from_slice(&data[..len]);
        }
        // SAFETY: as above.
        unsafe { it.zero(cap - 3, 3) };
        model[cap - 3..].fill(0);
        // SAFETY: as above.
        assert_eq!(unsafe { it.bytes(0, cap as u64) }.unwrap(), &model[..]);
        // SAFETY: as above.
        assert!(unsafe { it.bytes(cap as u64 - 1, 2) }.is_none(), "past the chunk is none");
        // SAFETY: made here, no one else holds it.
        unsafe { chunk.free() };
    }
}
