//! An object store (docs/v2/plan/design/std.md, "The rest of the set", `Blobs`): bytes kept
//! under the BLAKE3 of what they are, a [`BlobId`]. Made from a [`Dir`] ([`Blobs::beneath`]),
//! over [`Kv`]: a blob is the key that spells its id in hex, so everything [`crate::kv`] says
//! of durability holds here, with the same three forms of put and delete. A blob put
//! unsynced and torn by a crash is refused by its next get ([`BlobsKind::Corrupt`]), never
//! handed back.
//!
//! A get checks what it read against the id it was asked for and refuses bytes that are not
//! it ([`BlobsKind::Corrupt`]): the store is never trusted for what it did not verify.
//!
//! **Provider.** Real, over [`Dir`], as [`Kv`]'s.

use alloc::alloc::Global;
use core::fmt;

use blaze_mem::Frame;

use crate::{
    Stopped,
    cx::Cx,
    fs::{Dir, Durable, ReadOnly, ReadWrite, Rights, Unsynced},
    kv::{Key, KeyError, Kv, KvError},
    mem::{ByteLen, Bytes, String, Vec},
};

/// What a blob is: the BLAKE3 of its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId([u8; 32]);

impl BlobId {
    /// The id of `bytes`.
    pub fn of(bytes: &[u8]) -> BlobId {
        BlobId(blaze_ids::b3::hash(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The id a key spells in hex, if it spells one.
    fn from_key(key: &Key) -> Option<BlobId> {
        let hex = key.as_str().as_bytes();
        if hex.len() != 64 {
            return None;
        }
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut id = [0u8; 32];
        for (byte, pair) in id.iter_mut().zip(hex.chunks_exact(2)) {
            let [high, low] = pair else { return None };
            *byte = digit(*high)? << 4 | digit(*low)?;
        }
        Some(BlobId(id))
    }

    /// The key that spells it in hex: 64 hex digits, which every key's rule takes. With no
    /// room for the hex, [`BlobsKind::Stopped`]; a key's rule refusing it would be
    /// [`BlobsKind::Key`], the two rules disagreeing.
    fn key(&self) -> Result<Key, BlobsKind> {
        let stopped = |error| BlobsKind::Stopped(Stopped::from(error));
        // alloc: once per call, the id's hex, until the key copies it.
        let mut hex = String::with_capacity_in(64, Global).map_err(stopped)?;
        for byte in self.0 {
            hex.push(nibble(byte >> 4)).map_err(stopped)?;
            hex.push(nibble(byte & 0xf)).map_err(stopped)?;
        }
        Key::new(hex.as_str()).map_err(BlobsKind::Key)
    }
}

/// The hex digit of `half`, a value below 16.
fn nibble(half: u8) -> char {
    match half {
        0..=9 => char::from(b'0' + half),
        _ => char::from(b'a' + (half & 0xf) - 10),
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({self})")
    }
}

/// What a blob call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlobsOp {
    Get,
    Has,
    Put,
    Delete,
    List,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum BlobsKind {
    /// The store under it failed, saying this.
    Store(KvError),
    /// The bytes stored under the id are not its: they hash to `found`.
    Corrupt { found: BlobId },
    /// There was no room: for the list, or for the hex of the blob's id.
    Stopped(Stopped),
    /// The hex of the blob's id is no key: the id's rule and the key's disagree, a bug.
    Key(KeyError),
    /// The store said it added bytes from `start` and `into` holds only `held`.
    Short { start: usize, held: usize },
}

/// A blob call that failed: the call, the blob (none for a list) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct BlobsError {
    pub op: BlobsOp,
    pub blob: Option<BlobId>,
    pub kind: BlobsKind,
}

impl fmt::Display for BlobsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = self.op;
        match &self.blob {
            Some(blob) => write!(f, "bstd::blobs {op:?} of blob {blob}: ")?,
            None => write!(f, "bstd::blobs {op:?} of the store: ")?,
        }
        match &self.kind {
            BlobsKind::Corrupt { found } => write!(f, "the bytes stored under it hash to {found}"),
            BlobsKind::Store(under) => write!(f, "{under}"),
            BlobsKind::Stopped(stopped) => write!(f, "{stopped}"),
            BlobsKind::Key(error) => write!(f, "its hex is no key ({error}): the id's rule and the key's disagree"),
            BlobsKind::Short { start, held } => write!(
                f,
                "the store said it read bytes from {start} of the buffer, which holds {held} (a store's bug)"
            ),
        }
    }
}

impl core::error::Error for BlobsError {}

/// An object store beneath a directory: from [`Blobs::beneath`].
pub struct Blobs<#[blaze::linear] P: Rights> {
    kv: Kv<P>,
}

impl<#[blaze::linear] P: Rights> Clone for Blobs<P> {
    fn clone(&self) -> Blobs<P> {
        // clone: authority held, copied: refcounts.
        Blobs { kv: self.kv.clone() }
    }
}

impl<#[blaze::linear] P: Rights> fmt::Debug for Blobs<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blobs({:?})", self.kv)
    }
}

impl<#[blaze::linear] P: Rights> Blobs<P> {
    /// The store whose blobs are the files of `dir`, with `dir`'s rights.
    pub fn beneath(dir: &Dir<P>) -> Blobs<P> {
        Blobs { kv: Kv::beneath(dir) }
    }

    /// The same store, read only.
    pub fn read_only(&self) -> Blobs<ReadOnly> {
        Blobs { kv: self.kv.read_only() }
    }

    /// The blob `id` added to `into`, checked against `id`: how many bytes, or `None` when
    /// the store has no such blob. On an error `into` is as it was.
    pub fn get(&self, cx: &Cx, id: &BlobId, into: &mut Bytes<'_>) -> Result<Option<ByteLen>, BlobsError> {
        let refuse = |kind| BlobsError { op: BlobsOp::Get, blob: Some(*id), kind };
        let start = into.len();
        let key = id.key().map_err(refuse)?;
        let Some(read) = self.kv.get(cx, &key, into).map_err(|error| refuse(BlobsKind::Store(error)))? else {
            return Ok(None);
        };
        let Some((_, got)) = into.as_slice().split_at_checked(start) else {
            return Err(refuse(BlobsKind::Short { start, held: into.len() }));
        };
        let found = BlobId::of(got);
        if found != *id {
            into.truncate(start);
            return Err(refuse(BlobsKind::Corrupt { found }));
        }
        Ok(Some(read))
    }

    /// Whether the store has the blob `id`.
    pub fn has(&self, cx: &Cx, id: &BlobId) -> Result<bool, BlobsError> {
        let refuse = |kind| BlobsError { op: BlobsOp::Has, blob: Some(*id), kind };
        let key = id.key().map_err(refuse)?;
        self.kv.has(cx, &key).map_err(|error| refuse(BlobsKind::Store(error)))
    }

    /// The id of every blob in the store, sorted, in `frame`.
    pub fn list<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<BlobId, &'a Frame>, BlobsError> {
        let refuse = |kind| BlobsError { op: BlobsOp::List, blob: None, kind };
        let keys = self.kv.list(cx, frame).map_err(|error| refuse(BlobsKind::Store(error)))?;
        let mut ids = Vec::new_in(frame);
        for id in keys.iter().filter_map(BlobId::from_key) {
            ids.push(id).map_err(|error| refuse(BlobsKind::Stopped(Stopped::from(error))))?;
        }
        Ok(ids)
    }
}

impl Blobs<ReadWrite> {
    /// `bytes` kept, as durably as `durable` says: their id. [`Blobs::put_unsynced`] or
    /// [`Blobs::put_synced`], chosen at run time.
    pub fn put(&self, cx: &Cx, bytes: &[u8], durable: Durable) -> Result<BlobId, BlobsError> {
        match durable.unsynced() {
            Some(unsynced) => self.put_unsynced(cx, bytes, unsynced),
            None => self.put_synced(cx, bytes),
        }
    }

    /// `bytes` kept with nothing synced: their id. It never waits for the device; a crash
    /// may lose the blob or tear it, and a get of a torn one is [`BlobsKind::Corrupt`].
    pub fn put_unsynced(&self, cx: &Cx, bytes: &[u8], unsynced: Unsynced) -> Result<BlobId, BlobsError> {
        let id = BlobId::of(bytes);
        let refuse = |kind| BlobsError { op: BlobsOp::Put, blob: Some(id), kind };
        let key = id.key().map_err(refuse)?;
        match self.kv.put_unsynced(cx, &key, bytes, unsynced) {
            Ok(()) => Ok(id),
            Err(error) => Err(refuse(BlobsKind::Store(error))),
        }
    }

    /// `bytes` kept: their id. Durable once this returns, as [`Kv::put_synced`] is; a blob
    /// the store has already is written again as the same bytes. It waits for the device.
    pub fn put_synced(&self, cx: &Cx, bytes: &[u8]) -> Result<BlobId, BlobsError> {
        let id = BlobId::of(bytes);
        let refuse = |kind| BlobsError { op: BlobsOp::Put, blob: Some(id), kind };
        let key = id.key().map_err(refuse)?;
        match self.kv.put_synced(cx, &key, bytes) {
            Ok(()) => Ok(id),
            Err(error) => Err(refuse(BlobsKind::Store(error))),
        }
    }

    /// The blob `id` gone from the store, as durably as `durable` says; gone already is
    /// done. [`Blobs::delete_unsynced`] or [`Blobs::delete_synced`], chosen at run time.
    pub fn delete(&self, cx: &Cx, id: &BlobId, durable: Durable) -> Result<(), BlobsError> {
        match durable.unsynced() {
            Some(unsynced) => self.delete_unsynced(cx, id, unsynced),
            None => self.delete_synced(cx, id),
        }
    }

    /// The blob `id` gone from the store, with nothing synced: a crash may bring it back.
    pub fn delete_unsynced(&self, cx: &Cx, id: &BlobId, unsynced: Unsynced) -> Result<(), BlobsError> {
        let refuse = |kind| BlobsError { op: BlobsOp::Delete, blob: Some(*id), kind };
        let key = id.key().map_err(refuse)?;
        self.kv.delete_unsynced(cx, &key, unsynced).map_err(|error| refuse(BlobsKind::Store(error)))
    }

    /// The blob `id` gone from the store, and a crash keeps it gone. It waits for the device.
    pub fn delete_synced(&self, cx: &Cx, id: &BlobId) -> Result<(), BlobsError> {
        let refuse = |kind| BlobsError { op: BlobsOp::Delete, blob: Some(*id), kind };
        let key = id.key().map_err(refuse)?;
        self.kv.delete_synced(cx, &key).map_err(|error| refuse(BlobsKind::Store(error)))
    }
}
