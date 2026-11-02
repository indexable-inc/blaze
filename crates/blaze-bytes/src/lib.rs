//! A file's bytes and its text, held without a copy (docs/v2/plan/design/frontend.md item 6,
//! docs/v2/plan/design/restore.md, docs/v2/plan/lanes/frontend/text.md). [`Bytes`] are a heap
//! `Arc<[u8]>`, a range of something that keeps them in place (the daemon's mapped image), or a
//! `'static` slice; a [`Text`] is [`Bytes`] checked to be UTF-8 once ([`text_of`]) and lent as
//! `&str`. Cloning either takes one count, never a copy. They sit below the parse tree, so a
//! file's bytes in the VFS, its text in the database and the source of its tree are one
//! allocation, or one range of the image, from the disk to the parse.
//!
//! The crate's one unsafe module (docs/v2/plan/design/unsafe.md). The unsafe lines are the
//! owner's bytes lent as a `&'static [u8]` held beside the owner, [`Bytes::held`]'s contract
//! for an owner this crate cannot name, the `&[u8]` to `&str` view of checked bytes, and
//! [`Text::shared`]'s cast of a heap `Arc<[u8]>` to `Arc<str>`. The safe versions check UTF-8
//! on every read, or copy every file restored from the image.
//!
//! Costs: a [`Bytes`] or [`Text`] is 40 bytes (the slice, and the owner's tag and pointer);
//! its deref is the slice, one load of a pointer and a length. The check is std's, after the
//! ASCII the bytes start with is skipped 64 bytes at a time.

use std::{borrow::Borrow, fmt, hash, ops::Deref, str::Utf8Error, sync::Arc};

pub mod le;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// Bytes, held: their own `Arc`, a range its owner keeps in place, or a `'static` slice. The
/// same immutable bytes on every deref, for as long as any clone lives. `Send + Sync`.
#[derive(Clone)]
pub struct Bytes {
    /// Borrowed from `owner`, which keeps them at one address, unwritten, for as long as it
    /// is held: an `Arc`'s allocation, what [`Bytes::held`] was given, or nothing (`'static`).
    bytes: &'static [u8],
    owner: Owner,
}

#[derive(Clone)]
enum Owner {
    Heap(Arc<[u8]>),
    // dyn: the owner of an image's range, named by the crate that maps it; only ever dropped
    Held(#[allow(dead_code)] Arc<dyn Send + Sync>),
    Static,
}

impl Bytes {
    /// `bytes`, which `owner` keeps in place: a range of the daemon's mapped image, held by
    /// what maps it (blaze-vfs's tree, its page groups checked first).
    ///
    /// # Safety
    ///
    /// `bytes` must stay valid, at one address and unwritten, for as long as `owner` lives, as
    /// the bytes of a read-only mapping of a file that is only ever replaced by renaming
    /// another over it do while the mapping is held.
    // dyn: as `Owner::Held`; one call per file read from the image
    pub unsafe fn held(bytes: *const [u8], owner: Arc<dyn Send + Sync>) -> Bytes {
        // SAFETY: the caller keeps `bytes` valid and unwritten for as long as `owner` lives,
        // and this value holds `owner` for as long as it lends them.
        let bytes = unsafe { &*bytes };
        Bytes { bytes, owner: Owner::Held(owner) }
    }

    /// `bytes`, which live as long as the program.
    pub const fn from_static(bytes: &'static [u8]) -> Bytes {
        Bytes { bytes, owner: Owner::Static }
    }

    /// The bytes' own `Arc`, all of it, for bytes made from one; `None` for a range something
    /// else holds and for a `'static` slice.
    pub fn shared(&self) -> Option<&Arc<[u8]>> {
        match &self.owner {
            Owner::Heap(shared) => Some(shared),
            Owner::Held(_) | Owner::Static => None,
        }
    }

    /// Whether the two are the same bytes in memory (not only equal ones).
    pub fn ptr_eq(&self, other: &Bytes) -> bool {
        self.bytes.as_ptr() == other.bytes.as_ptr() && self.bytes.len() == other.bytes.len()
    }
}

impl From<Arc<[u8]>> for Bytes {
    fn from(shared: Arc<[u8]>) -> Bytes {
        let bytes: *const [u8] = &*shared;
        // SAFETY: an `Arc`'s allocation never moves and is not written while it is shared;
        // this value holds a count of it for as long as it lends the bytes.
        let bytes = unsafe { &*bytes };
        Bytes { bytes, owner: Owner::Heap(shared) }
    }
}

impl Default for Bytes {
    fn default() -> Bytes {
        Bytes::from_static(b"")
    }
}

impl Deref for Bytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self.bytes
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        self.bytes
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Bytes {}

impl hash::Hash for Bytes {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.owner {
            Owner::Heap(_) => "heap",
            Owner::Held(_) => "held",
            Owner::Static => "static",
        };
        write!(f, "Bytes({} bytes, {kind})", self.bytes.len())
    }
}

/// Bytes that are UTF-8, lent as `&str`: checked once, by [`text_of`], or made from a `str`.
/// Cloning it takes one count of the bytes. Equality, hashing and formatting are the `str`'s.
#[derive(Clone, Default)]
pub struct Text {
    /// UTF-8: checked by [`text_of`] or made from a `str`, and immutable for as long as a
    /// count of them is held.
    bytes: Bytes,
}

/// `bytes` as text: the same bytes, nothing copied; or where they stop being UTF-8.
pub fn text_of(bytes: Bytes) -> Result<Text, Utf8Error> {
    check(&bytes)?;
    Ok(Text { bytes })
}

impl Text {
    /// `text`, which lives as long as the program.
    pub const fn from_static(text: &'static str) -> Text {
        Text { bytes: Bytes::from_static(text.as_bytes()) }
    }

    pub fn as_str(&self) -> &str {
        self
    }

    /// The bytes it is.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    /// Whether the two are the same text in memory (not only equal ones).
    pub fn ptr_eq(&self, other: &Text) -> bool {
        self.bytes.ptr_eq(&other.bytes)
    }

    /// The text as an `Arc<str>` of the same allocation, one count taken, for text made from
    /// an `Arc`; `None` for a range something else holds (the mapped image), which no
    /// `Arc<str>` can be without a copy, and for a `'static` text.
    pub fn shared(&self) -> Option<Arc<str>> {
        let shared = self.bytes.shared()?;
        assert!(
            shared.as_ptr() == self.bytes.as_ptr() && shared.len() == self.bytes.len(),
            "heap `Bytes` are their whole `Arc`"
        );
        let raw: *const [u8] = Arc::into_raw(Arc::clone(shared));
        // SAFETY: `raw` is from `Arc::<[u8]>::into_raw`, and `str` has `[u8]`'s layout (size,
        // alignment and length metadata), which is what `Arc::from_raw` asks of a pointer of
        // another type. The `Arc`'s bytes are this text's (asserted just above), which are
        // UTF-8 (`Text`'s invariant), and while it is shared nothing can get them mutably, so
        // they stay UTF-8 for as long as the new `Arc` lives.
        Some(unsafe { Arc::from_raw(raw as *const str) })
    }
}

impl From<Arc<str>> for Text {
    /// The same allocation, nothing copied or checked.
    fn from(text: Arc<str>) -> Text {
        Text { bytes: Bytes::from(Arc::<[u8]>::from(text)) }
    }
}

impl From<&str> for Text {
    /// A copy of `text` in an `Arc` of its own.
    fn from(text: &str) -> Text {
        Text::from(Arc::<str>::from(text))
    }
}

impl From<String> for Text {
    /// `text` moved into an `Arc` of its own (one copy, as `Arc::<str>::from` makes).
    fn from(text: String) -> Text {
        Text::from(Arc::<str>::from(text))
    }
}

impl Deref for Text {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        // SAFETY: a `Text`'s bytes are UTF-8: checked by `text_of` or made from a `str`, the
        // only ways to make one. `Bytes` lends the same immutable bytes on every deref, kept
        // alive by the count `self` holds.
        unsafe { std::str::from_utf8_unchecked(&self.bytes) }
    }
}

impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        self
    }
}

impl Borrow<str> for Text {
    fn borrow(&self) -> &str {
        self
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        **self == **other
    }
}

impl Eq for Text {}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        **self == *other
    }
}

impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        **self == **other
    }
}

impl hash::Hash for Text {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

/// [`std::str::from_utf8`]'s answer for `bytes`: the ASCII they start with skipped 64 bytes at a
/// time (nearly all source is ASCII, and an OR of eight words vectorizes to a load and a test
/// per 32 or 64 bytes, at memory bandwidth), then std's check of the rest. The prefix ends
/// where a char starts, so the rest is UTF-8 exactly when the whole is; a refusal is asked of
/// the whole again, for where it stops.
fn check(bytes: &[u8]) -> Result<(), Utf8Error> {
    match std::str::from_utf8(&bytes[ascii_prefix(bytes)..]) {
        Ok(_) => Ok(()),
        Err(_) => std::str::from_utf8(bytes).map(drop),
    }
}

/// The length of the whole 64-byte chunks of ASCII `bytes` start with.
fn ascii_prefix(bytes: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let (chunks, _) = bytes.as_chunks::<64>();
    let ascii = chunks
        .iter()
        .take_while(|chunk| {
            chunk.as_chunks::<8>().0.iter().fold(0, |any, word| any | u64::from_ne_bytes(*word))
                & HIGH
                == 0
        })
        .count();
    ascii * 64
}
