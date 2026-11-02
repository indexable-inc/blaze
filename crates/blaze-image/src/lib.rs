//! The daemon's image (docs/v2/plan/design/restore.md): the database's memos, values, edges
//! and fingerprints, the interners, the VFS tree and the plane's history in one file, mapped
//! and used in place. Restoring is mapping the file and checking its spine; nothing is
//! deserialized, rebuilt or rehashed, and a page is read when something first reads it.
//!
//! Layout, little-endian, in page groups of `1 << group_shift` bytes (64 KiB by default):
//!
//! - group 0 is the **spine**: a [`HEADER`]-byte header (magic, [`LAYOUT`], the image's hash,
//!   the file's length, the counts), one 64-byte entry per section (its [`SectionId`], its
//!   owner's [`SchemaHash`], where it is, how long, which groups), then the CRC32C of each
//!   group of the CRC table. The image's hash is the BLAKE3 of the spine with the hash zeroed,
//!   so it commits to every group's CRC and through them to every byte. Opening checks it and
//!   nothing else.
//! - each section starts at a group boundary and fills whole groups, zero-padded;
//! - the CRC table last: one CRC32C per data group, in groups of its own.
//!
//! [`Image::group`], [`Image::range`] and [`Image::slice`] check each group they read the first
//! time it is read: its CRC against the table's entry, after the table's group against the
//! spine. A group that fails is [`Corrupt`], naming its section and group, every time it is
//! read; its owner drops what lives in it and computes it again from its inputs. Nothing is
//! read from a group that failed.
//!
//! The image is written whole by [`Builder`] and written to disk as one
//! `blaze_core::Effect::Write`, which writes a temporary file, syncs it and renames it over the
//! old one: a `kill -9` or a power cut leaves the old image or the new one. A file cut short
//! some other way is refused by its length, and one with bytes changed by its hash or a CRC.
//!
//! Cost: open reads the spine (128 bytes, 64 a section, 4 per GiB of image at 64 KiB groups)
//! and hashes it once; it allocates two bits a group. A read of a checked group is one relaxed
//! load of its bit; the first read of a group is one CRC32C of it in the CPU's CRC
//! instructions, about 3 µs for 64 KiB.

mod build;
mod image;
mod raw;
mod spine;

#[cfg(test)]
mod tests;

/// Counts allocations so the read paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;


use std::fmt;

use blaze_format::SchemaHash;
use blaze_ids::ContentId;

pub use crate::build::{Builder, SectionWriter, TooLarge};
pub use blaze_ids::crc32c::crc32c;
pub use crate::image::{Image, Stable};
pub use crate::raw::{bytes_of, cast};

/// How a whole-image pass (CRCs at write, a section checked whole at restore) spreads its work:
/// `jobs(n, job)` runs `job(0)` to `job(n - 1)` on as many workers as it has and returns when
/// all are done. The daemon passes its pool (`blaze_graph::pool`'s `run_jobs` has this shape);
/// [`serial`] runs them on the calling thread.
// dyn: the pool's interface, one call a 1 MiB job, never a value
pub type Jobs<'a> = &'a (dyn Fn(usize, &(dyn Fn(usize) + Sync)) + Sync);

/// [`Jobs`] on the calling thread.
// dyn: as `Jobs`
pub fn serial(n: usize, job: &(dyn Fn(usize) + Sync)) {
    (0..n).for_each(job);
}
pub use crate::spine::{
    DEFAULT_GROUP_SHIFT, HEADER, LAYOUT, MAGIC, MAX_GROUP_SHIFT, MIN_GROUP_SHIFT, SECTION_ENTRY,
    hash_of,
};

/// Which section of the image: who owns it and what it holds. The owner of a section is the
/// only reader of its bytes and the one who recomputes what a [`Corrupt`] group of it held.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct SectionId(pub u32);

impl SectionId {
    /// The database's memos (`blaze-query`).
    pub const DB_MEMOS: SectionId = SectionId(0x0100);
    /// The database's dependency edges, with the fingerprint each saw.
    pub const DB_EDGES: SectionId = SectionId(0x0101);
    /// The database's open-addressed key table.
    pub const DB_TABLE: SectionId = SectionId(0x0102);
    /// The database's inputs.
    pub const DB_INPUTS: SectionId = SectionId(0x0103);
    /// The database's values, key bytes and paths.
    pub const DB_BLOB: SectionId = SectionId(0x0104);
    /// The symbol table of `intern`: texts, order keys, the lookup index, the free ids.
    pub const SYMBOLS: SectionId = SectionId(0x0200);
    /// The plane's history: what tasks of each kind held (`blaze-graph`).
    pub const PLANE_HISTORY: SectionId = SectionId(0x0400);
    /// The query database's interned rows, at their IDs (`blaze_query::db`), so IDs stay
    /// stable across restarts.
    pub const QUERY_INTERNED: SectionId = SectionId(0x0500);
    /// The query database's frozen graph (`blaze_query::db`).
    pub const QUERY_GRAPH: SectionId = SectionId(0x0501);
    /// The library files' parsed syntax trees (lane tree-store, `base_db::trees::SCHEMA`).
    pub const SYNTAX_TREES: SectionId = SectionId(0x0600);
    /// The VFS's tree of nodes and names (`blaze-vfs`).
    pub const VFS_TREE: SectionId = SectionId::vfs(0);
    /// The VFS's file bytes.
    pub const VFS_BLOBS: SectionId = SectionId::vfs(1);
    /// The VFS's watchers' metadata and journal cursors.
    pub const VFS_DISKS: SectionId = SectionId::vfs(2);

    const VFS: u32 = 0x0300;
    const INTERNED: u32 = 0x1_0000;

    /// Part `part` of the VFS tree (`blaze-vfs`): its nodes, names and the journal's position.
    pub const fn vfs(part: u8) -> SectionId {
        SectionId(SectionId::VFS | part as u32)
    }

    /// The table of `intern`'s mapped interner of tag `tag` (`intern::Mapped`).
    pub const fn interned(tag: u16) -> SectionId {
        SectionId(SectionId::INTERNED | tag as u32)
    }

    /// What the section holds, for errors.
    pub fn name(self) -> &'static str {
        match self {
            SectionId::DB_MEMOS => "the database's memos",
            SectionId::DB_EDGES => "the database's edges",
            SectionId::DB_TABLE => "the database's key table",
            SectionId::DB_INPUTS => "the database's inputs",
            SectionId::DB_BLOB => "the database's values",
            SectionId::SYMBOLS => "the symbol table",
            SectionId::PLANE_HISTORY => "the plane's history",
            SectionId::QUERY_INTERNED => "the query database's interned rows",
            SectionId::QUERY_GRAPH => "the query graph",
            SectionId::SYNTAX_TREES => "the library syntax trees",
            SectionId(id) if id & !0xff == SectionId::VFS => "the VFS tree",
            SectionId(id) if id & !0xffff == SectionId::INTERNED => "an interned table",
            _ => "a section this blaze does not know",
        }
    }
}

impl fmt::Display for SectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (section {:#x})", self.name(), self.0)
    }
}

/// Where a section is in its image: what [`Image::section`] hands its owner, and every read
/// names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Section {
    pub id: SectionId,
    pub schema: SchemaHash,
    /// Where its first byte is, from the file's start: a group boundary.
    pub at: u64,
    /// How many bytes it holds; its last group is zero past them.
    pub len: u64,
    /// Its first group, counted from the first group after the spine.
    pub first: u32,
    pub groups: u32,
}

/// Why a file is not an image this blaze opens. Nothing of it is used.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpenError {
    /// Not an image, or one cut short: a wrong magic, a length that is not the file's.
    Header(&'static str),
    /// An image of another layout: written by another blaze.
    Layout { found: SchemaHash },
    /// A spine that is not the one its hash names: a torn or changed header.
    Hash { found: ContentId, wanted: ContentId },
    /// The section entry `index` of the spine is not a section of this image.
    Section { index: u32, what: &'static str },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Header(what) => write!(f, "not a blaze image: {what}"),
            OpenError::Layout { found } => {
                write!(f, "an image of layout {} where {} was wanted", found.0, LAYOUT.0)
            }
            OpenError::Hash { found, wanted } => {
                write!(f, "an image whose spine hashes to {found}, not the {wanted} it names")
            }
            OpenError::Section { index, what } => write!(f, "the image's section {index}: {what}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// Why an owner cannot use its section: it starts that state empty, naming why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The image has no such section: it was written without it.
    Missing { id: SectionId },
    /// The section was written by another layout of its owner.
    Schema { id: SectionId, found: SchemaHash, wanted: SchemaHash },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Missing { id } => write!(f, "the image has no {id}"),
            Refused::Schema { id, found, wanted } => {
                write!(f, "the image's {id} is of schema {} where {} was wanted", found.0, wanted.0)
            }
        }
    }
}

impl std::error::Error for Refused {}

/// A read of a section that cannot be trusted: the group it falls in (or, for [`Cause::Bounds`],
/// the read itself) is dropped by the section's owner and what it held computed again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Corrupt {
    pub section: SectionId,
    /// The group in the section, from 0.
    pub group: u32,
    pub cause: Cause,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// The group's bytes are not those its CRC32C was taken of.
    Crc { found: u32, wanted: u32 },
    /// The CRC table's group `table_group` holding this group's CRC is not the one the spine
    /// names, so no group whose CRC it holds can be checked.
    CrcTable { table_group: u32, found: u32, wanted: u32 },
    /// A read of `len` bytes at `at` of the section, which is shorter: the owner's records
    /// name bytes it never wrote.
    Bounds { at: u64, len: u64 },
    /// A read of a value aligned to `align` at `at` of the section, which is not.
    Misaligned { at: u64, align: u32 },
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Corrupt { section, group, cause } = self;
        match cause {
            Cause::Crc { found, wanted } => write!(
                f,
                "{section}, group {group}, is corrupt: its CRC32C is {found:#010x}, not {wanted:#010x}"
            ),
            Cause::CrcTable { table_group, found, wanted } => write!(
                f,
                "{section}, group {group}, cannot be checked: the CRC table's group {table_group} \
                 has CRC32C {found:#010x}, not {wanted:#010x}"
            ),
            Cause::Bounds { at, len } => {
                write!(f, "{section} is read for {len} bytes at {at}, past its end (group {group})")
            }
            Cause::Misaligned { at, align } => {
                write!(f, "{section} is read at {at} for a value aligned to {align} (group {group})")
            }
        }
    }
}

impl std::error::Error for Corrupt {}
