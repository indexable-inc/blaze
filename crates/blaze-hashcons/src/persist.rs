//! Interned values persist by content (docs/v2/plan/lanes/format.md section 5): an
//! [`Interned`] is written as its value's bytes, and bytes read back are made into a value and
//! interned again, never trusted to name an index or an address of another run.
//!
//! A pack holds every value of one interner, each as a buffer of its own at a 16-byte
//! boundary, in content ID order with the content ID it had beside it: the same values give the
//! same bytes in every run. Loading one checks each value's content ID against its entry before
//! interning it.

use std::mem::{offset_of, size_of};

use blaze_format::{
    ALIGN, FormatError, Part, Persist, Record, SchemaHash, Slice, Writer, open_as, open_verified, read_verified,
    schema_of,
};
use blaze_ids::ContentId;

use crate::{Consed, HashCons, Interned, content_of};
use records::{PackEntry, PackRoot};

/// An interned value is its value's bytes: the same schema, the same buffer, read in place as
/// the value is. Reading never interns; [`HashCons::load`] does.
impl<T: Consed + Persist> Persist for Interned<'static, T> {
    const SCHEMA: SchemaHash = T::SCHEMA;
    type Ref<'a> = T::Ref<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        self.get().write(out)
    }

    fn read(buffer: &[u8]) -> Result<T::Ref<'_>, FormatError> {
        T::read(buffer)
    }
}

/// A hash-consed value made again from its bytes.
pub trait Load: Consed + Persist {
    /// The interners its interned parts are made in again (`()` for a value with none).
    type Parts: Copy;
    /// The value `view` reads as, each interned part interned again in `parts`.
    fn load(view: <Self as Persist>::Ref<'_>, parts: Self::Parts) -> Self;
}

/// `record!` writes each record's `unsafe impl Record`, asserting it has no padding.
#[allow(unsafe_code)]
mod records {
    use blaze_format::{SchemaHash, Slice, record};
    use blaze_ids::ContentId;

    record! {
        #[derive(Clone, Copy)]
        pub(super) struct PackEntry: 1 {
            pub(super) content: ContentId,
            pub(super) bytes: Slice<u8>,
        }
    }

    record! {
        #[derive(Clone, Copy)]
        pub(super) struct PackRoot: 1 {
            /// The schema of every value in the pack.
            pub(super) schema: SchemaHash,
            pub(super) entries: Slice<PackEntry>,
        }
    }
}

/// The schema of a pack's buffer; the values' own is in its root.
pub const PACK_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("blaze v2 hashcons pack"), Part::U64(1), Part::Schema(<PackRoot as Record>::SCHEMA)]);

impl<T: Consed + Persist> HashCons<T> {
    /// Every value, as one buffer: [`HashCons::sorted`]'s order, so equal interners write equal
    /// bytes whatever order their values were interned in.
    pub fn write_pack(&self) -> Vec<u8> {
        let values = self.sorted();
        let mut out = Writer::new(PACK_SCHEMA);
        let mut scratch = Writer::new(T::SCHEMA);
        // clone: the header each value's scratch buffer starts again from, once per pack
        let header = scratch.bytes.clone();
        let mut placed = Vec::with_capacity(values.len());
        for value in &values {
            let root = value.get().write(&mut scratch);
            let mut bytes = scratch.finish(root);
            let pad = out.bytes.len().next_multiple_of(ALIGN) - out.bytes.len();
            out.push_slice(&[0u8; ALIGN][..pad]);
            let at = out.push_slice(&bytes);
            placed.push((value.content(), at, bytes.len()));
            bytes.clear();
            bytes.extend_from_slice(&header);
            scratch = Writer { bytes };
        }
        let first = out.next::<PackEntry>();
        let size = size_of::<PackEntry>() as u64;
        let entries: Vec<PackEntry> = placed
            .iter()
            .enumerate()
            .map(|(i, &(content, at, len))| {
                let field = first + i as u64 * size + offset_of!(PackEntry, bytes) as u64;
                PackEntry { content, bytes: Slice::between(field, at, len) }
            })
            // alloc: the pack's entry table, once per pack written
            .collect();
        let written = out.push_slice(&entries);
        debug_assert_eq!(written, first);
        let at = out.next::<PackRoot>();
        let entries = match entries.is_empty() {
            true => Slice::EMPTY,
            false => Slice::between(at + offset_of!(PackRoot, entries) as u64, first, entries.len()),
        };
        let written = out.push(&PackRoot { schema: T::SCHEMA, entries });
        debug_assert_eq!(written, at);
        out.finish(at)
    }
}

impl<T: Load> HashCons<T> {
    /// The value `buffer` holds (bytes this process wrote), interned.
    pub fn load(&self, buffer: &[u8], parts: T::Parts) -> Result<Interned<'_, T>, FormatError> {
        let view = T::read(buffer)?;
        Ok(self.intern(T::load(view, parts)))
    }

    /// [`HashCons::load`] of bytes from outside this process, after checking they are the
    /// buffer `key` names.
    pub fn load_verified(
        &self,
        buffer: &[u8],
        key: ContentId,
        parts: T::Parts,
    ) -> Result<Interned<'_, T>, FormatError> {
        let view = read_verified::<T>(buffer, key)?;
        Ok(self.intern(T::load(view, parts)))
    }

    /// Interns every value of a pack this process wrote ([`HashCons::write_pack`]); how many it
    /// held.
    pub fn load_pack(&self, buffer: &[u8], parts: T::Parts) -> Result<usize, FormatError> {
        let root = open_as::<PackRoot>(buffer, PACK_SCHEMA)?;
        self.load_entries(root, buffer, parts)
    }

    /// [`HashCons::load_pack`] of bytes from outside this process, after checking they are
    /// the buffer `key` names.
    pub fn load_pack_verified(&self, buffer: &[u8], key: ContentId, parts: T::Parts) -> Result<usize, FormatError> {
        let root = open_verified::<PackRoot>(buffer, PACK_SCHEMA, key)?;
        self.load_entries(root, buffer, parts)
    }

    fn load_entries(&self, root: &PackRoot, buffer: &[u8], parts: T::Parts) -> Result<usize, FormatError> {
        if root.schema != T::SCHEMA {
            return Err(FormatError::Schema { found: root.schema, wanted: T::SCHEMA });
        }
        let entries = root.entries.follow(buffer)?;
        let mut last: Option<ContentId> = None;
        for entry in entries {
            let at = (entry as *const PackEntry as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
            if last.is_some_and(|last| last >= entry.content) {
                return Err(FormatError::Invalid { at, what: "a pack whose values are not in content ID order" });
            }
            let value = T::load(T::read(entry.bytes.follow(buffer)?)?, parts);
            let content = content_of(&value);
            if content != entry.content {
                return Err(FormatError::Invalid { at, what: "a pack value whose content ID is not its entry's" });
            }
            self.intern_content(content, value);
            last = Some(entry.content);
        }
        Ok(entries.len())
    }
}
