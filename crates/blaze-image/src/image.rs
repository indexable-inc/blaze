//! A mapped image, read in place: [`Image::open`] reads only the spine, and each group is checked
//! by its CRC32C the first time a read reaches it.

use std::{
    ops::Range,
    sync::{Arc, atomic::Ordering::Relaxed},
};

use blaze_bytes::Bytes;
use blaze_format::{Record, SchemaHash};
use blaze_ids::{ContentId, bits::AtomicBits, crc32c::crc32c};

use crate::{
    Cause, Corrupt, OpenError, Refused, Section, SectionId, raw,
    spine::{self, Spine},
};

/// Bytes at one address for as long as the value lives: an mmap of the image, or the bytes of a
/// test. [`Image`] reads them through a pointer taken once at open.
///
/// # Safety
///
/// Every call of [`Stable::bytes`] on a value returns the same slice (address and length), and
/// its bytes are never written while the value lives.
pub unsafe trait Stable: Send + Sync + 'static {
    fn bytes(&self) -> &[u8];
}

// SAFETY: a `Vec` owned by the image is never pushed to or written, so its buffer stays put.
unsafe impl Stable for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: a boxed slice never moves or changes length; it is not written through `&self`.
unsafe impl Stable for Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: a `'static` shared slice is one slice forever, and shared, so never written.
unsafe impl Stable for &'static [u8] {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: `Aligned` owns its blocks in a `Vec` it never changes after it is made.
unsafe impl Stable for blaze_format::Aligned {
    fn bytes(&self) -> &[u8] {
        self
    }
}

// SAFETY: a `Mapping` is what `Effect::Map` made (a read-only mmap of a file the store or the
// daemon only ever replaces by renaming another over it, never writes in place) or bytes
// copied into an `Arc` with no way to write them; its contract (blaze-core's `Mapping`) is that
// its bytes stay as they are while any clone lives, and `bytes` of either returns one slice.
// The daemon, the db and the VFS all open the mapped image as `Image<Mapping>`, and a section
// they keep is `(image.owner().clone(), image.whole_range(&section)?)`.
unsafe impl Stable for blaze_core::Mapping {
    fn bytes(&self) -> &[u8] {
        blaze_core::Mapping::bytes(self)
    }
}

/// An image, its spine checked. `Sync`: every read takes `&self`, and the only writes are each
/// group's bits, set once.
pub struct Image<S: Stable> {
    owner: S,
    spine: Spine,
    /// Bit `g` of the words: data group `g` matched its CRC. Then bit `groups + t`: CRC table
    /// group `t` matched its top CRC.
    checked: AtomicBits,
    /// The same bits for a group that did not match: read again, it fails again, with no CRC.
    bad: AtomicBits,
    pub(crate) touched: Touched,
}

/// Readers on any thread share one image (`Arc<Image<Mapping>>`).
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Image<blaze_core::Mapping>>();
    shared::<Image<Vec<u8>>>();
};

/// What reads of an image's bytes reached, for the tests that count them; nothing outside tests.
#[derive(Default)]
pub(crate) struct Touched {
    #[cfg(test)]
    pub(crate) bytes: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    pub(crate) end: std::sync::atomic::AtomicU64,
}

impl Touched {
    #[inline(always)]
    fn note(&self, at: u64, len: u64) {
        #[cfg(test)]
        {
            self.bytes.fetch_add(len, Relaxed);
            self.end.fetch_max(at + len, Relaxed);
        }
        let _ = (at, len);
    }
}

/// Bit `index` of the checked and bad sets: an image has at most `u32::MAX` groups and as many
/// CRC table groups, so it fits a `usize` on every target blaze runs on.
fn bit_of(index: u64) -> usize {
    usize::try_from(index).expect("an image's groups fit a usize")
}

impl<S: Stable> Image<S> {
    /// The image held by `owner`, after reading its spine and nothing else: the header, the
    /// section table and the CRC table's CRCs, checked against the spine's hash.
    pub fn open(owner: S) -> Result<Image<S>, OpenError> {
        let touched = Touched::default();
        let bytes = owner.bytes();
        let spine = spine::parse(bytes.len() as u64, |prefix| {
            touched.note(0, prefix as u64);
            &bytes[..prefix]
        })?;
        let bits = u64::from(spine.groups) + spine.tops.len() as u64;
        Ok(Image {
            owner,
            spine,
            checked: AtomicBits::new(bit_of(bits)),
            bad: AtomicBits::new(bit_of(bits)),
            touched,
        })
    }

    /// The image's hash: the BLAKE3 of its spine, which holds every group's CRC.
    pub fn hash(&self) -> ContentId {
        self.spine.hash
    }

    pub fn len(&self) -> u64 {
        self.spine.len
    }

    pub fn group_len(&self) -> u64 {
        self.spine.group_len()
    }

    pub fn sections(&self) -> &[Section] {
        &self.spine.sections
    }

    /// Section `id`, if the image has it and it is of `schema`.
    pub fn section(&self, id: SectionId, schema: SchemaHash) -> Result<Section, Refused> {
        let section = self.spine.sections.iter().find(|it| it.id == id).ok_or(Refused::Missing { id })?;
        match section.schema == schema {
            true => Ok(*section),
            false => Err(Refused::Schema { id, found: section.schema, wanted: schema }),
        }
    }

    /// `len` bytes of the file at `at`, which the spine says are in it.
    fn span(&self, at: u64, len: u64) -> &[u8] {
        #[cfg(test)]
        self.touched.note(at, len);
        &self.owner.bytes()[at as usize..(at + len) as usize]
    }

    /// Group `group` of `section`, checked: its bytes up to the section's end.
    pub fn group(&self, section: &Section, group: u32) -> Result<&[u8], Corrupt> {
        if group >= section.groups {
            let at = u64::from(group) * self.group_len();
            return Err(Corrupt { section: section.id, group, cause: Cause::Bounds { at, len: 1 } });
        }
        self.ensure(section, group)?;
        let at = u64::from(group) * self.group_len();
        let len = (section.len - at).min(self.group_len());
        Ok(self.span(section.at + at, len))
    }

    /// `len` bytes at `at` of `section`, each group they fall in checked.
    #[inline]
    pub fn range(&self, section: &Section, at: u64, len: u64) -> Result<&[u8], Corrupt> {
        let shift = self.spine.shift;
        let end = at.checked_add(len).filter(|&end| end <= section.len);
        let Some(end) = end else {
            let group = (at >> shift).min(u64::from(u32::MAX)) as u32;
            return Err(Corrupt { section: section.id, group, cause: Cause::Bounds { at, len } });
        };
        if len == 0 {
            return Ok(&[]);
        }
        let (first, last) = ((at >> shift) as u32, ((end - 1) >> shift) as u32);
        for group in first..=last {
            self.ensure(section, group)?;
        }
        Ok(self.span(section.at + at, len))
    }

    /// `count` `T`s at `at` of `section`, read in place: `T` a plain record, `at` aligned for it.
    #[inline]
    pub fn slice<T: Record>(&self, section: &Section, at: u64, count: u64) -> Result<&[T], Corrupt> {
        let bytes = count.checked_mul(size_of::<T>() as u64).unwrap_or(u64::MAX);
        let bytes = self.range(section, at, bytes)?;
        raw::cast::<T>(bytes).ok_or(Corrupt {
            section: section.id,
            group: (at >> self.spine.shift) as u32,
            cause: Cause::Misaligned { at, align: align_of::<T>() as u32 },
        })
    }

    /// The `T` at `at` of `section`.
    #[inline]
    pub fn value<T: Record>(&self, section: &Section, at: u64) -> Result<&T, Corrupt> {
        Ok(&self.slice::<T>(section, at, 1)?[0])
    }

    /// Groups `groups` of `section` checked now (each once ever, as any read checks it), the
    /// first that fails returned: what a caller that checks a whole section on many workers
    /// gives each worker, since one core's CRC32C (about 8 bytes a cycle) is a tenth of what the
    /// machine's memory can stream.
    pub fn check(&self, section: &Section, groups: Range<u32>) -> Result<(), Corrupt> {
        groups.take_while(|&group| group < section.groups).try_for_each(|group| self.ensure(section, group))
    }

    /// All of `section`, every group checked: for a section its owner reads whole at start.
    /// One core checks it; [`Image::check`] spreads a large one over workers first.
    pub fn whole(&self, section: &Section) -> Result<&[u8], Corrupt> {
        self.range(section, 0, section.len)
    }

    /// Where all of `section` is in the owner's bytes, every group checked: for an owner that
    /// keeps the section as its own mapping and range rather than a borrow of the image.
    pub fn whole_range(&self, section: &Section) -> Result<Range<usize>, Corrupt> {
        let bytes = self.whole(section)?;
        let start = section.at as usize;
        debug_assert!(bytes.is_empty() || bytes.as_ptr() == self.owner.bytes()[start..].as_ptr());
        Ok(start..start + bytes.len())
    }

    /// All of `section`, every group checked, as [`Bytes`] that keep the image alive: for an
    /// owner that keeps a section's bytes past a borrow of the image (the library syntax trees,
    /// the query graph), with no mapping layer of its own. They start 8-aligned, so 8-byte records
    /// read in place; an owner whose bytes don't place them so is refused as `Misaligned`.
    pub fn whole_bytes(self: &Arc<Self>, section: &Section) -> Result<Bytes, Corrupt> {
        let whole = self.whole(section)?;
        if whole.is_empty() {
            return Ok(Bytes::default());
        }
        if !whole.as_ptr().addr().is_multiple_of(8) {
            return Err(Corrupt { section: section.id, group: 0, cause: Cause::Misaligned { at: 0, align: 8 } });
        }
        let bytes: *const [u8] = whole;
        // SAFETY: the bytes are `self.owner`'s, which `Stable` keeps at one address and
        // unwritten for as long as the owner lives; the owner lives as long as the image, and
        // the `Bytes` hold the image's `Arc` for as long as they lend them.
        Ok(unsafe { Bytes::held(bytes, Arc::clone(self) as Arc<dyn Send + Sync>) })
    }

    /// Every group of every section checked now: each one that fails, in order. On this thread:
    /// for tests and a check by hand. A whole-image check on the daemon's path is
    /// [`Image::check`] over chunks of groups on its pool.
    pub fn verify(&self) -> Vec<Corrupt> {
        // alloc: the failures of a whole check by hand or in a test; the daemon uses `Image::check`
        let mut failed = Vec::new();
        for section in &self.spine.sections {
            for group in 0..section.groups {
                if let Err(corrupt) = self.ensure(section, group) {
                    failed.push(corrupt);
                }
            }
        }
        failed
    }

    /// Whether group `group` of `section` was read and matched its CRC.
    pub fn is_checked(&self, section: &Section, group: u32) -> bool {
        self.checked.contains(bit_of(u64::from(section.first + group)), Relaxed)
    }

    /// Group `group` of `section` checked, once.
    #[inline]
    fn ensure(&self, section: &Section, group: u32) -> Result<(), Corrupt> {
        // Relaxed: the image's bytes are never written after it is opened, so a set bit orders
        // nothing; a thread that misses one checks the group again and sets it again.
        if self.checked.contains(bit_of(u64::from(section.first + group)), Relaxed) {
            return Ok(());
        }
        self.check_group(section, group)
    }

    #[cold]
    #[inline(never)]
    fn check_group(&self, section: &Section, group: u32) -> Result<(), Corrupt> {
        let global = section.first + group;
        let corrupt = |cause| Corrupt { section: section.id, group, cause };
        let wanted = self.table_entry(global).map_err(corrupt)?;
        let at = self.spine.group_len() * (1 + u64::from(global));
        let found = crc32c(self.span(at, self.spine.group_len()));
        let bit = bit_of(u64::from(global));
        if found != wanted {
            self.bad.insert(bit, Relaxed);
            return Err(corrupt(Cause::Crc { found, wanted }));
        }
        self.checked.insert(bit, Relaxed);
        Ok(())
    }

    /// The CRC the table holds for data group `global`, its table group checked first.
    fn table_entry(&self, global: u32) -> Result<u32, Cause> {
        let group = self.spine.group_len();
        let entry_at = u64::from(global) * 4;
        let table_group = (entry_at / group) as u32;
        let index = u64::from(self.spine.groups) + u64::from(table_group);
        let bit = bit_of(index);
        if !self.checked.contains(bit, Relaxed) {
            let wanted = self.spine.tops[table_group as usize];
            let found = crc32c(self.span(self.spine.crc_at + u64::from(table_group) * group, group));
            if found != wanted {
                self.bad.insert(bit, Relaxed);
                return Err(Cause::CrcTable { table_group, found, wanted });
            }
            self.checked.insert(bit, Relaxed);
        }
        let bytes = self.span(self.spine.crc_at + entry_at, 4);
        Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
    }

    /// Whether a read of group `group` of `section` failed before.
    pub fn is_bad(&self, section: &Section, group: u32) -> bool {
        self.bad.contains(bit_of(u64::from(section.first + group)), Relaxed)
    }

    /// The bytes the image is held in, for an owner that hands them on (the database keeps
    /// its mapping for values it reads later).
    pub fn owner(&self) -> &S {
        &self.owner
    }
}
