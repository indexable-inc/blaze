//! The store's files outside its packs, and how bytes get to a path: whether a file's data
//! reached the disk is in its type, and how far a write goes toward the disk is an argument.
//!
//! Measured 2026-11-10 (xfs, a 162 MB executable): a whole-file write with no sync, 29 ms; a
//! clone (`FICLONE`) of a file written back, 1 ms; a clone of the file just written, 1.3 s,
//! because a file system that shares blocks writes the source's dirty pages out first and
//! waits for them. "Write it, then clone it" cost the sync it had been written to avoid, three
//! times an edit. So here that sequence cannot be written:
//!
//! - A file the store wrote is a [`Written`] until its write-back is known complete
//!   ([`blaze_core::Effect::WriteBack`]'s completion), then a [`Flushed`]. A clone is asked
//!   only through [`Flushed::clone_to`]; nothing else in the store makes an
//!   [`Effect::Clone`]. The bytes the store holds in its records are written wherever they
//!   are wanted whatever the file's state ([`write`]): a copy shares no block and reads no
//!   file, so nothing of the file's dirty data is forced out by it.
//! - Every write and rename made here takes a [`Durable`], with no default ([`write`],
//!   [`rename`]).
//! - [`crate::Store::publish`] is the one entry that makes a path hold some [`Content`]: it
//!   picks a write, a clone or a derive (a clone of a base and its ranges written) from what
//!   the content is, and its place's span says which (`how`). [`crate::Store::place`] is a
//!   caller of it.
//!
//! This is bstd's file API (docs/v2/plan/design/std.md, "Files: whether bytes reached the disk
//! is in the type") as far as a sans-IO core has it, to move there unchanged in what it
//! promises. Where it differs, and why: the store is a state machine on effects, so `flush`
//! gives the effect and [`Written::flushed`] is called with its completion, where bstd's
//! `written.flush(cx)` is one call that waits; a copy is [`write`] of bytes the caller
//! gives, on no handle, where bstd has `copy_to` on both, because the store keeps a value's
//! bytes in its records and never reads a whole file back to copy it; and a hard link is never chosen, since an output must be replaceable and writable by its
//! user without the store's file changing, and a link is one inode, mode and all.

#![forbid(unsafe_code)]

use blaze_core::{Durable, Effect, FileName, Tag};
use blaze_ids::ContentId;

use crate::Source;

/// A file the store wrote (or found, or made of a clone and writes) whose data may still be
/// only in memory: its write-back is not known complete. It cannot be cloned: there is no
/// method for it. It becomes a [`Flushed`] with the completion of its [`Written::flush`].
#[derive(Debug)]
pub(crate) struct Written {
    name: FileName,
}

/// A file whose data is on its device as far as the store knows: it saw the file's write-back
/// complete and has written nothing to it since, or the file is one its commit synced. The
/// one thing a clone is made from.
#[derive(Clone, Debug)] // clone: a file's name, a reference count: each clone in flight holds its source's
pub(crate) struct Flushed {
    name: FileName,
}

impl Written {
    /// The file at `name`, which this store just wrote, derived or took over.
    pub(crate) fn wrote(name: FileName) -> Written {
        Written { name }
    }

    pub(crate) fn name(&self) -> &FileName {
        &self.name
    }

    /// Its write-back begun ([`Durable::Started`]): the effect whose completion,
    /// [`blaze_core::Done::WrittenBack`], makes it a [`Flushed`]. Nothing waits for it but
    /// what wants to clone the file.
    pub(crate) fn flush(&self, tag: Tag) -> Effect {
        // clone: the effect owns the name of the file it writes back
        Effect::WriteBack { file: self.name.clone(), tag }
    }

    /// The file once its [`Written::flush`] completed well, and nothing was written since.
    pub(crate) fn flushed(self) -> Flushed {
        Flushed { name: self.name }
    }
}

impl Flushed {
    /// A file the store's commit synced (a value kept as a file of its own, named by the
    /// head): durable, so clean.
    pub(crate) fn synced(name: FileName) -> Flushed {
        Flushed { name }
    }

    /// `to` made a clone of it, sharing its blocks: the only clone the store asks for, and
    /// of a file with no data left to write back, so the file system shares at once.
    pub(crate) fn clone_to(&self, to: FileName, tag: Tag) -> Effect {
        // clone: the effect owns the name of the file it clones
        Effect::Clone { from: self.name.clone(), to, tag }
    }
}

/// `bytes` as the whole of `file`, replaced in one step for a reader, taken as far toward the
/// disk as `durable` says. [`Durable::Started`] is the write of [`Durable::No`]: the caller
/// begins the write-back of the [`Written`] it then holds.
pub(crate) fn write(file: FileName, bytes: Vec<u8>, durable: Durable, tag: Tag) -> Effect {
    match durable {
        Durable::No | Durable::Started => Effect::WriteUnsynced { file, bytes, tag },
        // durable: asked for by the caller in its argument, which says why where it is given
        Durable::Yes => Effect::Write { file, bytes, tag },
    }
}

/// `from` renamed over `to` in one step for a reader, the new name taken as far toward the
/// disk as `durable` says.
pub(crate) fn rename(from: FileName, to: FileName, durable: Durable, tag: Tag) -> Effect {
    match durable {
        Durable::No | Durable::Started => Effect::RenameUnsynced { from, to, tag },
        // durable: asked for by the caller in its argument, which says why where it is given
        Durable::Yes => Effect::Rename { from, to, tag },
    }
}

/// What a path is to hold ([`crate::Store::publish`]).
pub(crate) enum Content {
    /// These bytes: written where they go. `how`: `write`.
    Bytes(Vec<u8>),
    /// `from`'s, `len` bytes that hash to `content`: cloned under a staged name and renamed
    /// onto the path. `of`, the whole file of the store's it is, counted as cloned until the
    /// clone ends. `how`: `clone`.
    File { from: Flushed, content: ContentId, of: Option<Source>, len: u64 },
    /// `base`'s (the whole file `base_id`, counted as cloned until the clone ends) with the
    /// ranges of the derived value `id` written over it and cut to `len`: cloned under a
    /// staged name, written into and renamed onto the path. `how`: `derive`.
    Patch { base: Flushed, base_id: ContentId, id: ContentId, len: u64 },
}

impl Content {
    /// What the place's span says of it.
    pub(crate) fn how(&self) -> &'static str {
        match self {
            Content::Bytes(_) => "write",
            Content::File { .. } => "clone",
            Content::Patch { .. } => "derive",
        }
    }
}

/// An output being made of a clone of its base with a derived value's ranges written
/// ([`Content::Patch`]), by the tag its place was asked with.
pub(crate) struct Patching {
    pub(crate) to: FileName,
    /// The hidden name beside `to` it is made under ([`crate::Store::staging_name`]).
    pub(crate) temp: FileName,
    pub(crate) base_id: ContentId,
    pub(crate) id: ContentId,
    pub(crate) len: u64,
    /// The writes and the cut still in flight, and the first that failed.
    pub(crate) left: u32,
    pub(crate) failed: Option<String>,
    /// The bytes its ranges wrote.
    pub(crate) wrote: u64,
    /// Bytes written into the store that this place is the first to report.
    pub(crate) written: u64,
}
