//! Writing a buffer: [`Writer`], over a buffer in the caller's allocator, and, with the `alloc`
//! feature, what is defined over `alloc`'s `Vec` ([`Persist`], [`to_bytes`], [`Aligned`]).
//!
//! A writer's buffer is named by where it is allocated ([`WriterIn`]): an allocator `A` itself
//! names `alloc`'s `Vec<u8, A>` (the `alloc` feature; what every writer in the tree used before
//! this crate, so `Writer`, `Writer<A>`, `Writer::new_in(schema, alloc)` and
//! `Writer::with_buffer(schema, vec)` mean what they meant), and [`Own`]`<A>` names
//! `blaze_own::Vec<u8, A>`, the collection a crate on `core` alone writes into. One writer, one
//! set of methods; the buffer type is a parameter of it.

use core::alloc::Allocator;
use core::mem::{align_of, offset_of};

#[cfg(feature = "alloc")]
use alloc::alloc::Global;

use crate::{ALIGN, HEADER_LEN, Header, MAGIC, Record, SchemaHash, raw};

/// Where a [`Writer`]'s buffer is allocated, which names the buffer: an allocator `A` (with the
/// `alloc` feature) for `alloc`'s `Vec<u8, A>`, or [`Own`]`<A>` for `blaze_own::Vec<u8, A>`.
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

/// The struct is written twice, with and without its default: a default type must exist, and
/// `Global` exists only with `alloc`.
macro_rules! writer {
    ($($default:ty)?) => {
        /// Appends records to one buffer, aligned, and gives each a place for the [`Rel`]s of
        /// others. Padding between records is zeros, so equal values written in the same order
        /// are equal bytes. The buffer is in `P` ([`WriterIn`]): with the `alloc` feature,
        /// `Writer` is `Writer<Global>`, whose buffer is `alloc`'s `Vec<u8>`.
        ///
        /// [`Rel`]: crate::Rel
        pub struct Writer<P: WriterIn $(= $default)?> {
            pub bytes: P::Buffer,
        }
    };
}

#[cfg(feature = "alloc")]
writer!(Global);
#[cfg(not(feature = "alloc"))]
writer!();

fn align_up(at: usize, align: usize) -> usize {
    assert!(align <= ALIGN, "a record aligned to more than {ALIGN} bytes");
    at.next_multiple_of(align)
}

#[cfg(feature = "alloc")]
impl Writer {
    pub fn new(schema: SchemaHash) -> Writer {
        Writer::new_in(schema, Global)
    }
}

impl<P: WriterIn> Writer<P> {
    /// A writer whose buffer is in `place`: [`Writer::finish`] hands back a `P::Buffer`.
    pub fn new_in(schema: SchemaHash, place: P) -> Writer<P> {
        Writer::with_buffer(schema, place.buffer(4096))
    }

    /// A writer over `bytes`, cleared and keeping its capacity: a caller that writes many
    /// values hands each [`Writer::finish`]'s buffer back, and once it has grown a write
    /// allocates nothing. The buffer's type names the writer's `P`.
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

/// A value a query returns, or the store keeps: written once, read in place.
#[cfg(feature = "alloc")]
pub trait Persist: Send + Sync + 'static {
    const SCHEMA: SchemaHash;
    /// The value as read from a buffer, borrowing it.
    type Ref<'a>: Copy;
    /// Writes the value, returning where its root is.
    fn write(&self, out: &mut Writer) -> u64;
    /// The value in `buffer`, checked as [`crate::open_as`] checks. Bytes this process did not
    /// write are read through [`read_verified`], so a `read` may rely on the buffer being one
    /// some blaze wrote whole.
    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, crate::FormatError>;
}

/// [`Persist::read`] of bytes from outside this process, after checking they are the buffer
/// `key` names.
#[cfg(feature = "alloc")]
pub fn read_verified<P: Persist>(
    buffer: &[u8],
    key: blaze_ids::ContentId,
) -> Result<P::Ref<'_>, crate::FormatError> {
    crate::verify(buffer, key)?;
    P::read(buffer)
}

/// `value` as a buffer of its own.
#[cfg(feature = "alloc")]
pub fn to_bytes<P: Persist>(value: &P) -> alloc::vec::Vec<u8> {
    let mut out = Writer::new(P::SCHEMA);
    let root = value.write(&mut out);
    out.finish(root)
}

#[cfg(feature = "alloc")]
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub(crate) struct Block([u8; ALIGN]);

/// Owned bytes at an address aligned to [`ALIGN`]: what a buffer is read from when it is not
/// mmapped (a `Vec<u8>` is aligned to 1).
#[cfg(feature = "alloc")]
#[derive(Clone)]
pub struct Aligned {
    blocks: alloc::vec::Vec<Block>,
    len: usize,
}

#[cfg(feature = "alloc")]
impl Aligned {
    pub fn copy_from(bytes: &[u8]) -> Aligned {
        let count = bytes.len().div_ceil(ALIGN);
        let mut blocks = alloc::vec::Vec::with_capacity_in(count, Global);
        blocks.resize(count, Block([0; ALIGN]));
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(ALIGN)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        Aligned { blocks, len: bytes.len() }
    }
}

#[cfg(feature = "alloc")]
impl core::ops::Deref for Aligned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        raw::blocks_bytes(&self.blocks, self.len)
    }
}

#[cfg(feature = "alloc")]
impl core::fmt::Debug for Aligned {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Aligned").field("len", &self.len).finish()
    }
}
