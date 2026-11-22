//! Writing a buffer: [`Writer`], over a buffer in an allocator the caller names, made by
//! [`Writer::new`] from [`WriterOpts`]; [`Persist`], the one trait a kept value writes and reads
//! itself by; and [`AlignedIn`], owned bytes at the format's alignment, in the caller's allocator.
//!
//! A writer's buffer is named by where it is allocated ([`WriterIn`]): [`Own`]`<A>` names
//! `blaze_own::Vec<u8, A>`, the collection every writer moves to. With the `alloc` feature an
//! allocator `A` itself also names `alloc`'s `Vec<u8, A>`, for the crates whose record buffers
//! are still `alloc`'s (blaze-surface's `RecordBytes`, blaze-run's events, blaze-expand's and
//! blaze-keyed's payload writers): the allocator is named there too, never defaulted. One writer,
//! one set of methods; the buffer type is a parameter of it, with no default (D170 C1).
//!
//! Nothing here allocates but the writer's own buffer, in the allocator its opts name, and
//! [`AlignedIn::copy_from`], in the allocator it is handed. The read side ([`Persist::read`],
//! [`read_verified`]) allocates nothing: a value is read in place, borrowing its buffer.
//!
//! The two items under `alloc` that name `Global` ([`Writer::on_the_global_heap_until_ported`]
//! and [`Aligned`]) exist only for callers not yet ported; each is deprecated in its doc and
//! names what replaces it.
#![blaze::sans_io]

use core::alloc::Allocator;
use core::mem::{align_of, offset_of};

#[cfg(feature = "alloc")]
use alloc::alloc::Global;

use crate::{ALIGN, HEADER_LEN, Header, MAGIC, Record, SchemaHash, raw};

/// Where a [`Writer`]'s buffer is allocated, which names the buffer: [`Own`]`<A>` for
/// `blaze_own::Vec<u8, A>`, or (with the `alloc` feature) an allocator `A` for `alloc`'s
/// `Vec<u8, A>`.
pub trait WriterIn: Sized {
    type Buffer: WriteBuffer<In = Self>;
    /// An empty buffer in this place, with room for `capacity` bytes.
    fn buffer(self, capacity: usize) -> Self::Buffer;
}

/// A growable buffer of bytes a [`Writer`] appends to, and where it is allocated: the
/// operations a writer needs, each the collection's own.
pub trait WriteBuffer: Sized {
    type In: WriterIn<Buffer = Self>;
    fn len(&self) -> usize;
    fn clear(&mut self);
    fn reserve(&mut self, additional: usize);
    fn extend_from_slice(&mut self, bytes: &[u8]);
    /// Makes the length `len`, new bytes `fill`.
    fn resize(&mut self, len: usize, fill: u8);
    fn as_mut_slice(&mut self) -> &mut [u8];
}

/// A writer's buffer in blaze's own collection, `blaze_own::Vec<u8, A>`, allocated in `A`.
#[derive(Clone, Copy, Debug)]
pub struct Own<A>(pub A);

impl<A: Allocator> WriterIn for Own<A> {
    type Buffer = blaze_own::Vec<u8, A>;
    fn buffer(self, capacity: usize) -> blaze_own::Vec<u8, A> {
        blaze_own::Vec::with_capacity_in(capacity, self.0)
    }
}

impl<A: Allocator> WriteBuffer for blaze_own::Vec<u8, A> {
    type In = Own<A>;
    fn len(&self) -> usize {
        blaze_own::Vec::len(self)
    }
    fn clear(&mut self) {
        blaze_own::Vec::clear(self);
    }
    fn reserve(&mut self, additional: usize) {
        blaze_own::Vec::reserve(self, additional);
    }
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        blaze_own::Vec::extend_from_slice(self, bytes);
    }
    fn resize(&mut self, len: usize, fill: u8) {
        blaze_own::Vec::resize(self, len, fill);
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        blaze_own::Vec::as_mut_slice(self)
    }
}

/// `alloc`'s `Vec<u8, A>` as a buffer, in the allocator `A` the caller names: kept until the
/// crates the module's comment names hold their record buffers in `blaze_own::Vec`, then
/// deleted with the `alloc` feature.
#[cfg(feature = "alloc")]
impl<A: Allocator> WriterIn for A {
    type Buffer = alloc::vec::Vec<u8, A>;
    fn buffer(self, capacity: usize) -> alloc::vec::Vec<u8, A> {
        alloc::vec::Vec::with_capacity_in(capacity, self)
    }
}

#[cfg(feature = "alloc")]
impl<A: Allocator> WriteBuffer for alloc::vec::Vec<u8, A> {
    type In = A;
    fn len(&self) -> usize {
        alloc::vec::Vec::len(self)
    }
    fn clear(&mut self) {
        alloc::vec::Vec::clear(self);
    }
    fn reserve(&mut self, additional: usize) {
        alloc::vec::Vec::reserve(self, additional);
    }
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        alloc::vec::Vec::extend_from_slice(self, bytes);
    }
    fn resize(&mut self, len: usize, fill: u8) {
        alloc::vec::Vec::resize(self, len, fill);
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        alloc::vec::Vec::as_mut_slice(self)
    }
}

/// Appends records to one buffer, aligned, and gives each a place for the [`Rel`]s of others.
/// Padding between records is zeros, so equal values written in the same order are equal bytes,
/// whatever allocator the buffer is in. The buffer is in `P` ([`WriterIn`]), which every writer
/// names: there is no default.
///
/// [`Rel`]: crate::Rel
pub struct Writer<P: WriterIn> {
    pub bytes: P::Buffer,
}

/// The room a writer's buffer is made with when its opts say `capacity: None`: what every writer
/// was made with before the opts existed. It changes no byte written, only when the buffer grows.
pub const WRITER_CAPACITY: usize = 4096;

/// What a [`Writer`] is made from ([`Writer::new`], rule 83 options-struct): every field is
/// written at every use, `Writer::new(WriterOpts { schema, alloc: Own(frame), capacity: None })`.
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct WriterOpts<P> {
    /// The schema the buffer's header names: its root record's type's (or the [`Persist`]
    /// type's). It is the schema part of the record's key (D162): the store files a buffer under
    /// `Key::of(schema, producer, inputs)` with this same value.
    pub schema: SchemaHash,
    /// Where the buffer is allocated ([`WriterIn`]): `Own(alloc)` for `blaze_own::Vec<u8, A>`.
    pub alloc: P,
    /// Bytes room is made for at once: `None` for [`WRITER_CAPACITY`], `Some(n)` for `n` (at
    /// least the header's). It changes no byte written.
    pub capacity: Option<usize>,
}

fn align_up(at: usize, align: usize) -> usize {
    assert!(align <= ALIGN, "a record aligned to more than {ALIGN} bytes");
    at.next_multiple_of(align)
}

/// DEPRECATED: a writer on the global heap, for the callers that had no allocator in scope when
/// `Writer`'s `Global` default was removed (D170 C1). Each such call is listed in the wave's
/// report with the conversion that removes it; no new caller may use it. The replacement is
/// [`Writer::new`] with the caller's allocator.
#[cfg(feature = "alloc")]
impl Writer<Global> {
    pub fn on_the_global_heap_until_ported(schema: SchemaHash) -> Writer<Global> {
        Writer::new(WriterOpts { schema, alloc: Global, capacity: None })
    }
}

impl<P: WriterIn> Writer<P> {
    /// The writer `opts` describe: its buffer in `opts.alloc`, its header naming `opts.schema`.
    /// [`Writer::finish`] hands back a `P::Buffer`.
    pub fn new(opts: WriterOpts<P>) -> Writer<P> {
        let capacity = opts.capacity.unwrap_or(WRITER_CAPACITY).max(HEADER_LEN);
        Writer::with_buffer(opts.schema, opts.alloc.buffer(capacity))
    }

    /// A writer over `bytes`, a buffer [`Writer::finish`] handed back, cleared and keeping its
    /// capacity: a caller that writes many values hands each finished buffer back, and once it
    /// has grown a write allocates nothing. The buffer's type names the writer's `P`, and its
    /// allocator is the one it was made in. A conversion of that buffer, not a second way to make
    /// a writer from nothing.
    pub fn with_buffer<B>(schema: SchemaHash, mut bytes: B) -> Writer<P>
    where
        B: WriteBuffer<In = P>,
        P: WriterIn<Buffer = B>,
    {
        bytes.clear();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&schema.0.0);
        bytes.extend_from_slice(&[0; 16]);
        debug_assert_eq!(bytes.len(), HEADER_LEN);
        Writer { bytes }
    }

    /// Where the next `T` pushed will be, from the buffer's start: what a record's [`Rel`]s
    /// and [`Slice`]s are made from before it is pushed.
    ///
    /// [`Rel`]: crate::Rel
    /// [`Slice`]: crate::Slice
    pub fn next<T: Record>(&self) -> u64 {
        align_up(self.bytes.len(), align_of::<T>()) as u64
    }

    /// Pads with zeros to a multiple of `align`, at most [`ALIGN`], and says where that is.
    pub fn pad(&mut self, align: usize) -> u64 {
        let at = align_up(self.bytes.len(), align);
        self.bytes.resize(at, 0);
        at as u64
    }

    /// Where `value` was written, from the buffer's start.
    pub fn push<T: Record>(&mut self, value: &T) -> u64 {
        self.push_slice(core::slice::from_ref(value))
    }

    pub fn push_slice<T: Record>(&mut self, values: &[T]) -> u64 {
        let at = self.pad(align_of::<T>());
        self.bytes.extend_from_slice(raw::bytes_of(values));
        at
    }

    /// Where `bytes` were written, at a multiple of [`ALIGN`]: a buffer of this format held
    /// inside another (a record's payload) is then opened where it is, with no copy, since
    /// [`crate::open`] takes only a buffer aligned to [`ALIGN`].
    pub fn push_buffer(&mut self, bytes: &[u8]) -> u64 {
        let at = self.pad(ALIGN);
        self.bytes.extend_from_slice(bytes);
        at
    }

    /// Where `text` was written, for a [`crate::Str`] of `text.len()` bytes.
    pub fn push_str(&mut self, text: &str) -> u64 {
        self.push_slice(text.as_bytes())
    }

    /// The buffer, with `root` as its root record.
    pub fn finish(mut self, root: u64) -> P::Buffer {
        assert!(
            root >= HEADER_LEN as u64 && root < self.bytes.len() as u64,
            "the root is not a record of this buffer"
        );
        let len = self.pad(ALIGN);
        let at = offset_of!(Header, len);
        self.bytes.as_mut_slice()[at..at + 8].copy_from_slice(&len.to_le_bytes());
        let at = offset_of!(Header, root);
        self.bytes.as_mut_slice()[at..at + 8].copy_from_slice(&root.to_le_bytes());
        self.bytes
    }
}

/// A value a query returns, or the store keeps: written once, read in place. The one generic
/// persistence trait: a value writes itself into a writer the caller made, in the caller's
/// allocator, and reads itself from bytes it borrows, allocating nothing.
pub trait Persist: Send + Sync + 'static {
    /// The schema of the value's buffer: what its writer's [`WriterOpts::schema`] is, and the
    /// schema part of its key (D162).
    const SCHEMA: SchemaHash;
    /// The value as read from a buffer, borrowing it.
    type Ref<'a>: Copy;
    /// Writes the value into `out`, a writer of [`Persist::SCHEMA`] whose buffer is wherever
    /// its maker put it, returning where its root is. The same bytes whatever `P` is.
    fn write<P: WriterIn>(&self, out: &mut Writer<P>) -> u64;
    /// The value in `buffer`, checked as [`crate::open_as`] checks, read in place. Bytes this
    /// process did not write are read through [`read_verified`], so a `read` may rely on the
    /// buffer being one some blaze wrote whole.
    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, crate::FormatError>;
}

/// [`Persist::read`] of bytes from outside this process, after checking they are the buffer
/// `key` names. Allocates nothing.
pub fn read_verified<P: Persist>(
    buffer: &[u8],
    key: blaze_ids::ContentId,
) -> Result<P::Ref<'_>, crate::FormatError> {
    crate::verify(buffer, key)?;
    P::read(buffer)
}

/// `value` as a buffer of its own, in `alloc` ([`WriterIn`]: `Own(alloc)` for a
/// `blaze_own::Vec<u8, A>`).
pub fn to_bytes<T: Persist, P: WriterIn>(value: &T, alloc: P) -> P::Buffer {
    let mut out = Writer::new(WriterOpts { schema: T::SCHEMA, alloc, capacity: None });
    let root = value.write(&mut out);
    out.finish(root)
}

#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub(crate) struct Block([u8; ALIGN]);

/// Owned bytes at an address aligned to [`ALIGN`], in the allocator `A` the caller names: what a
/// buffer is read from when it is not mmapped (a `Vec<u8>` is aligned to 1). Copying into one is
/// the one allocation a reader of unaligned bytes makes; mapped or aligned bytes are read where
/// they are. Once the last [`Aligned`] is ported this is renamed `Aligned`.
#[derive(Clone)]
pub struct AlignedIn<A: Allocator> {
    blocks: blaze_own::Vec<Block, A>,
    len: usize,
}

impl<A: Allocator> AlignedIn<A> {
    /// `bytes`, copied into blocks of [`ALIGN`] bytes in `alloc`, the last one's tail zeros.
    pub fn copy_from(bytes: &[u8], alloc: A) -> AlignedIn<A> {
        let count = bytes.len().div_ceil(ALIGN);
        let mut blocks = blaze_own::Vec::with_capacity_in(count, alloc);
        blocks.resize(count, Block([0; ALIGN]));
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(ALIGN)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        AlignedIn { blocks, len: bytes.len() }
    }
}

impl<A: Allocator> core::ops::Deref for AlignedIn<A> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        raw::blocks_bytes(&self.blocks, self.len)
    }
}

impl<A: Allocator> core::fmt::Debug for AlignedIn<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedIn").field("len", &self.len).finish()
    }
}

/// DEPRECATED: [`AlignedIn`] on the global heap, for the callers that held one before the
/// allocator was named (D170 C1): about a hundred calls in blaze-keyed, blaze-expand,
/// blaze-surface, blaze-impls, blaze-scope, blaze-file-items, blaze-run, blaze-lir,
/// blaze-hashcons and blaze-format-tiers, each moved to [`AlignedIn::copy_from`] with its own
/// allocator by its crate's conversion. No new caller may use it.
#[cfg(feature = "alloc")]
#[derive(Clone)]
pub struct Aligned(AlignedIn<Global>);

#[cfg(feature = "alloc")]
impl Aligned {
    /// DEPRECATED: [`AlignedIn::copy_from`] with the caller's allocator replaces it.
    pub fn copy_from(bytes: &[u8]) -> Aligned {
        Aligned(AlignedIn::copy_from(bytes, Global))
    }
}

#[cfg(feature = "alloc")]
impl core::ops::Deref for Aligned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(feature = "alloc")]
impl core::fmt::Debug for Aligned {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Aligned").field("len", &self.0.len).finish()
    }
}
