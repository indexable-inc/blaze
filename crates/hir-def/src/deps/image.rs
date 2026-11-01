//! The image of a dependency crate: one [`blaze_format`] buffer, a [`Root`] naming the key the
//! image was written under and its parts, each the bytes of one encoding, sorted by tag.
//!
//! A part's bytes start at an address aligned to [`blaze_format::ALIGN`] in the buffer, so a
//! part that is a buffer of its own (the signatures) is opened in place. Finding a part is a
//! binary search over at most [`Part::ALL`] records; nothing is copied.

use std::{ops::Range, sync::Arc};

use base_db::ContentId;
use blaze_format::{Aligned, FormatError, Part as SchemaPart, Record, SchemaHash, Slice, Writer};

blaze_format::record! {
    /// One part: its tag and its bytes.
    #[derive(Clone, Copy, Debug)]
    struct PartRec: 1 {
        tag: u32,
        bytes: Slice<u8>,
    }
}

blaze_format::record! {
    /// The image: the key it was written under, then its parts in the order of their tags.
    #[derive(Clone, Copy)]
    struct Root: 1 {
        key: ContentId,
        parts: Slice<PartRec>,
    }
}

const _: () = {
    assert!(size_of::<PartRec>() == 12);
    assert!(size_of::<Root>() == 40);
};

/// The schema of an image buffer: its root record's, and the version of what the parts are.
pub const IMAGE_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    SchemaPart::Str("blaze v2 dependency image"),
    SchemaPart::U64(2),
    SchemaPart::Schema(<Root as Record>::SCHEMA),
]);

/// What holds the bytes of an image or a crate's reads as the store gave them: the store's own
/// mapping, held in place (`blaze_cas::Reader::pinned`, through the driver's adapter), or a
/// buffer of its own. The bytes start at a multiple of [`blaze_format::ALIGN`] (an image whose
/// parts are not aligned does not open) and neither move nor change while it lives.
pub trait Held: Send + Sync + 'static {
    fn bytes(&self) -> &[u8];
}

impl Held for Aligned {
    #[inline]
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// The bytes a store gave, held where it keeps them.
/// Its parts are read through [`ImageRef`].
// dyn: the store's holder of one image, one per crate per session
pub type ImageBuf = Arc<dyn Held>;

/// What a part of an image is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum Part {
    /// The crate's `DefMap` and `LocalDefMap`, as [`crate::nameres::frozen::encode`] writes them.
    DefMap = 0,
    /// The crate's lang items, as [`crate::nameres::frozen::encode_lang_items`] writes them.
    LangItems = 1,
    /// The signatures of the crate's items, a buffer of [`super::signatures`]'s schema.
    Signatures = 2,
}

impl Part {
    pub const ALL: [Part; 3] = [Part::DefMap, Part::LangItems, Part::Signatures];

    pub fn name(self) -> &'static str {
        match self {
            Part::DefMap => "def map",
            Part::LangItems => "lang items",
            Part::Signatures => "signatures",
        }
    }
}

/// Why an image does not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    Format(FormatError),
    /// The image was written under another key than the one it was found by.
    Key {
        found: ContentId,
        wanted: ContentId,
    },
    /// Its parts are not one of each of [`Part::ALL`], in order, with tags it knows.
    Parts(&'static str),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Format(error) => write!(f, "{error}"),
            ImageError::Key { found, wanted } => {
                write!(f, "written under key {found:?}, found by key {wanted:?}")
            }
            ImageError::Parts(what) => f.write_str(what),
        }
    }
}

impl From<FormatError> for ImageError {
    fn from(error: FormatError) -> ImageError {
        ImageError::Format(error)
    }
}

/// An image, read in place: each part a slice of the buffer.
#[derive(Clone, Copy)]
pub struct ImageRef<'a> {
    parts: [&'a [u8]; Part::ALL.len()],
}

impl<'a> ImageRef<'a> {
    /// The image in `buffer`, which must have been written under `key`: its header and schema
    /// checked, its root's key compared, and each part's bounds and alignment checked. The
    /// buffer's content hash is the store's to check, which it does on every read.
    pub fn open(buffer: &'a [u8], key: ContentId) -> Result<ImageRef<'a>, ImageError> {
        let root = blaze_format::open_as::<Root>(buffer, IMAGE_SCHEMA)?;
        if root.key != key {
            return Err(ImageError::Key { found: root.key, wanted: key });
        }
        let recs = root.parts.follow(buffer)?;
        if recs.len() != Part::ALL.len() {
            return Err(ImageError::Parts("not one part of each kind"));
        }
        let mut parts: [&'a [u8]; Part::ALL.len()] = [&[]; Part::ALL.len()];
        for ((rec, part), slot) in recs.iter().zip(Part::ALL).zip(&mut parts) {
            if rec.tag != part as u32 {
                return Err(ImageError::Parts("a part out of order or of no kind"));
            }
            let bytes = rec.bytes.follow(buffer)?;
            if !(bytes.as_ptr() as usize).is_multiple_of(blaze_format::ALIGN) && !bytes.is_empty() {
                return Err(ImageError::Parts("a part not aligned to 16 bytes"));
            }
            *slot = bytes;
        }
        Ok(ImageRef { parts })
    }

    /// The bytes of `part`, in place.
    #[inline]
    pub fn part(self, part: Part) -> &'a [u8] {
        self.parts[part as usize]
    }

    /// Where each part is in `buffer`, the buffer this was opened from: what reopening it
    /// with [`ImageRef::at`] takes, with no check again.
    pub(crate) fn ranges(self, buffer: &[u8]) -> [Range<usize>; Part::ALL.len()] {
        self.parts.map(|part| {
            let at = match part.is_empty() {
                true => 0,
                false => part.as_ptr() as usize - buffer.as_ptr() as usize,
            };
            at..at + part.len()
        })
    }

    /// The image whose parts are `ranges` of `buffer`, as [`ImageRef::ranges`] gave them for
    /// it: three slices, no check of the buffer.
    #[inline]
    pub(crate) fn at(buffer: &'a [u8], ranges: &[Range<usize>; Part::ALL.len()]) -> ImageRef<'a> {
        // clone: three `Range<usize>`s, which are not `Copy`
        ImageRef { parts: ranges.clone().map(|it| &buffer[it]) }
    }
}

/// The bytes of a part, borrowed from the image buffer they are in (a count of it, no copy) or
/// owned: what the frozen def maps and lang items read.
#[derive(Clone)]
pub enum PartBytes {
    Owned(Arc<[u8]>),
    In { buffer: ImageBuf, range: Range<usize> },
}

impl PartBytes {
    /// Part `part` of the image in `buffer` whose parts are `ranges` ([`ImageRef::ranges`]).
    pub fn of(
        buffer: &ImageBuf,
        ranges: &[Range<usize>; Part::ALL.len()],
        part: Part,
    ) -> PartBytes {
        // clone: a count of the image buffer, which the part borrows, and a `Range<usize>`
        PartBytes::In { buffer: buffer.clone(), range: ranges[part as usize].clone() }
    }
}

impl std::ops::Deref for PartBytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            PartBytes::Owned(bytes) => bytes,
            // clone: a `Range<usize>`, which is not `Copy`
            PartBytes::In { buffer, range } => &buffer.bytes()[range.clone()],
        }
    }
}

impl PartialEq for PartBytes {
    fn eq(&self, other: &PartBytes) -> bool {
        **self == **other
    }
}

impl Eq for PartBytes {}

impl std::hash::Hash for PartBytes {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl std::fmt::Debug for PartBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PartBytes({} bytes)", self.len())
    }
}

impl From<&[u8]> for PartBytes {
    fn from(bytes: &[u8]) -> PartBytes {
        PartBytes::Owned(bytes.into())
    }
}

impl From<Vec<u8>> for PartBytes {
    fn from(bytes: Vec<u8>) -> PartBytes {
        PartBytes::Owned(bytes.into())
    }
}

impl From<Arc<[u8]>> for PartBytes {
    fn from(bytes: Arc<[u8]>) -> PartBytes {
        PartBytes::Owned(bytes)
    }
}

/// The image of `key` whose parts are `parts`, one of each of [`Part::ALL`] in that order.
pub fn write_image(key: ContentId, parts: [&[u8]; Part::ALL.len()]) -> Vec<u8> {
    let total: usize = parts.iter().map(|it| it.len().next_multiple_of(blaze_format::ALIGN)).sum();
    let mut out = Writer::new(IMAGE_SCHEMA);
    out.bytes.reserve(total + 4 * blaze_format::ALIGN + size_of::<Root>());
    let mut placed = [(0u64, 0usize); Part::ALL.len()];
    for (at, bytes) in parts.iter().enumerate() {
        let start = out.bytes.len().next_multiple_of(blaze_format::ALIGN);
        out.bytes.resize(start, 0);
        placed[at] = (out.push_slice(bytes), bytes.len());
    }
    let recs_at = out.next::<PartRec>();
    let recs: [PartRec; Part::ALL.len()] = std::array::from_fn(|index| {
        let (bytes_at, len) = placed[index];
        let field = recs_at
            + (index * size_of::<PartRec>()) as u64
            + std::mem::offset_of!(PartRec, bytes) as u64;
        let bytes = match len {
            0 => Slice::EMPTY,
            len => Slice::between(field, bytes_at, len),
        };
        PartRec { tag: Part::ALL[index] as u32, bytes }
    });
    let written = out.push_slice(&recs);
    debug_assert_eq!(written, recs_at, "`Writer::next` is not where `push_slice` writes");
    let root_at = out.next::<Root>();
    let field = root_at + std::mem::offset_of!(Root, parts) as u64;
    let root = Root { key, parts: Slice::between(field, recs_at, recs.len()) };
    let written = out.push(&root);
    debug_assert_eq!(written, root_at, "`Writer::next` is not where `push` writes");
    out.finish(root_at)
}
