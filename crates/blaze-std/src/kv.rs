//! A key-value store (docs/v2/plan/design/std.md, "The rest of the set", `Kv`): get, put,
//! delete and list by a typed [`Key`], values as bytes in the caller's allocator. Made from a
//! [`Dir`] ([`Kv::beneath`]) and never reaching further than it: a lower capability makes the
//! higher one. It is what a host with no files may offer in their place.
//!
//! Rights are the directory's: a `Kv<ReadOnly>` gets and lists, a `Kv<ReadWrite>` also puts
//! and deletes, and [`Kv::read_only`] narrows.
//!
//! **Durability is said at every put and delete**, as for files ([`crate::fs`]): each has an
//! `_unsynced` form that never waits for the device, a `_synced` form that does, and a form
//! that takes a [`Durable`] and picks one at run time. There is no default.
//!
//! | call | after a crash the key holds |
//! |---|---|
//! | `put_synced` returned | exactly the value |
//! | `put_unsynced` returned | what it held durably before, or any prefix of the value |
//! | `delete_synced` returned | nothing |
//! | `delete_unsynced` returned | nothing, or what it held |
//!
//! A reader always sees a key's old value or its new, never a part, crash or no crash
//! between: a put replaces all at once.
//!
//! **Provider.** Real, over [`Dir`]: a key is a file of its name beneath the store's
//! directory, a put is a [`Dir::publish`] of the value's bytes (a temporary file beside the
//! key, renamed over it; its name starts with a `.`, which no key does, so one left by a
//! crash is never listed), and a delete is a remove. A host that offers a store directly (a
//! wasm worker's) would be a second provider; none exists yet.

use alloc::{borrow::ToOwned, string::String, vec::Vec};
use core::fmt;

use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    cx::Cx,
    fs::{Content, Dir, Durable, FsError, FsKind, ReadOnly, ReadWrite, Rights, Unsynced},
    mem::Bytes,
    path::Path,
};

/// The most bytes in a key.
pub const KEY_MOST: usize = 128;

/// A key: 1 to [`KEY_MOST`] bytes of letters, digits, `-`, `_` and `.`, not starting with a
/// `.`. So a key is one file's name as it is written, with no encoding between them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount
pub struct Key(Path);

/// What [`Key::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct KeyError {
    pub key: String,
    pub why: KeyWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyWhy {
    Empty,
    /// More than [`KEY_MOST`] bytes.
    TooLong,
    /// It starts with a `.`.
    Hidden,
    /// A byte that is no letter, digit, `-`, `_` or `.`.
    BadByte,
}

impl KeyWhy {
    const fn says(self) -> &'static str {
        match self {
            KeyWhy::Empty => "is empty",
            KeyWhy::TooLong => "is longer than 128 bytes",
            KeyWhy::Hidden => "starts with a `.`",
            KeyWhy::BadByte => "holds a byte that is no letter, digit, `-`, `_` or `.`",
        }
    }
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key {:?} {}", self.key, self.why.says())
    }
}

impl core::error::Error for KeyError {}

/// Why `raw` is no key, if it is not.
pub const fn refused(raw: &str) -> Option<KeyWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(KeyWhy::Empty);
    }
    if bytes.len() > KEY_MOST {
        return Some(KeyWhy::TooLong);
    }
    if bytes[0] == b'.' {
        return Some(KeyWhy::Hidden);
    }
    let mut at = 0;
    while at < bytes.len() {
        if !(bytes[at].is_ascii_alphanumeric() || bytes[at] == b'-' || bytes[at] == b'_' || bytes[at] == b'.') {
            return Some(KeyWhy::BadByte);
        }
        at += 1;
    }
    None
}

impl Key {
    /// `raw` as a key, or why it is none.
    ///
    /// The contract: a key is 1 to [`KEY_MOST`] bytes.
    pub fn new(raw: &str) -> (r: Result<Key, KeyError>)
        ensures match &r {
            Ok(key) => !key.as_str().is_empty() && key.as_str().len() <= KEY_MOST,
            Err(_) => true,
        }
    {
        if let Some(why) = refused(raw) {
            // alloc: an error's message.
            return Err(KeyError { key: raw.to_owned(), why });
        }
        Ok(Key(Path::new(raw).expect("a key is one part of a relative path")))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The file a key is, beneath its store's directory.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({:?})", self.as_str())
    }
}

impl TryFrom<&str> for Key {
    type Error = KeyError;

    fn try_from(raw: &str) -> Result<Key, KeyError> {
        Key::new(raw)
    }
}

/// What a store call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KvOp {
    Get,
    Has,
    Put,
    Delete,
    List,
}

/// A store call that failed: the call, the key (none for a list) and the file call under it,
/// which names the file and how it failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct KvError {
    pub op: KvOp,
    pub key: Option<Key>,
    pub under: FsError,
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(key) => write!(f, "bstd::kv {:?} of key {key}: {}", self.op, self.under),
            None => write!(f, "bstd::kv {:?} of the store: {}", self.op, self.under),
        }
    }
}

impl core::error::Error for KvError {}

impl KvError {
    /// The stop, if the wait under it stopped.
    pub fn stopped(&self) -> Option<&Stopped> {
        self.under.stopped()
    }
}

/// A key-value store beneath a directory: from [`Kv::beneath`].
pub struct Kv<#[blaze::linear] P: Rights> {
    dir: Dir<P>,
}

impl<#[blaze::linear] P: Rights> Clone for Kv<P> {
    fn clone(&self) -> Kv<P> {
        // clone: authority held, copied: refcounts.
        Kv { dir: self.dir.clone() }
    }
}

impl<#[blaze::linear] P: Rights> fmt::Debug for Kv<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Kv({:?})", self.dir)
    }
}

impl<#[blaze::linear] P: Rights> Kv<P> {
    /// The store whose keys are the files of `dir`, with `dir`'s rights.
    pub fn beneath(dir: &Dir<P>) -> Kv<P> {
        // clone: authority held, copied: refcounts.
        Kv { dir: dir.clone() }
    }

    /// The same store, read only.
    pub fn read_only(&self) -> Kv<ReadOnly> {
        Kv { dir: self.dir.read_only() }
    }

    fn refuse(op: KvOp, key: Option<&Key>, under: FsError) -> KvError {
        // clone: an error's name of its key, a refcount.
        KvError { op, key: key.cloned(), under }
    }

    /// The value of `key` added to `into`: how many bytes, or `None` when the store has no
    /// such key (then `into` is as it was).
    pub fn get(&self, cx: &Cx, key: &Key, into: &mut Bytes<'_>) -> Result<Option<usize>, KvError> {
        match self.dir.read_all(cx, key.path(), into) {
            Ok(read) => Ok(Some(read)),
            Err(error) if error.kind == FsKind::NotFound => Ok(None),
            Err(error) => Err(Self::refuse(KvOp::Get, Some(key), error)),
        }
    }

    /// Whether the store has `key`.
    pub fn has(&self, cx: &Cx, key: &Key) -> Result<bool, KvError> {
        match self.dir.stat(cx, key.path()) {
            Ok(found) => Ok(found.is_some_and(|meta| !meta.dir)),
            Err(error) => Err(Self::refuse(KvOp::Has, Some(key), error)),
        }
    }

    /// Every key of the store, sorted, in `frame`. A file beneath the directory that is no
    /// key's (in a directory of its own, or with a name no [`Key`] has) is not the store's
    /// and is not listed.
    pub fn list<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<Key, &'a Frame>, KvError> {
        let files = self.dir.list(cx, frame).map_err(|error| Self::refuse(KvOp::List, None, error))?;
        let mut keys = Vec::new_in(frame);
        for (path, _) in &files {
            let Ok(key) = Key::new(path.as_str()) else { continue };
            keys.try_reserve(1).map_err(|_| {
                let stopped = Stopped::OutOfMemory { arena: "bstd::kv list", asked: size_of::<Key>() };
                // clone: an error's name of the store's directory, a refcount.
                let under = FsError { op: crate::fs::FsOp::List, path: self.dir.name().clone(), kind: FsKind::Stopped(stopped) };
                Self::refuse(KvOp::List, None, under)
            })?;
            keys.push(key);
        }
        Ok(keys)
    }
}

impl Kv<ReadWrite> {
    /// `key` made to hold `value`, as durably as `durable` says: [`Kv::put_unsynced`] or
    /// [`Kv::put_synced`], chosen at run time.
    pub fn put(&self, cx: &Cx, key: &Key, value: &[u8], durable: Durable) -> Result<(), KvError> {
        match durable.unsynced() {
            Some(unsynced) => self.put_unsynced(cx, key, value, unsynced),
            None => self.put_synced(cx, key, value),
        }
    }

    /// `key` made to hold `value`, all at once for a reader, with nothing synced: it never
    /// waits for the device. After a crash the key holds what it held durably before, or any
    /// prefix of `value`.
    pub fn put_unsynced(&self, cx: &Cx, key: &Key, value: &[u8], unsynced: Unsynced) -> Result<(), KvError> {
        match self.dir.publish_unsynced(cx, key.path(), Content::bytes(value), unsynced) {
            Ok(_) => Ok(()),
            Err(error) => Err(Self::refuse(KvOp::Put, Some(key), error)),
        }
    }

    /// `key` made to hold `value`, durably and all at once: once this returns, a crash keeps
    /// it; before, a crash keeps the old value or the new, never a part. It waits for the
    /// device.
    pub fn put_synced(&self, cx: &Cx, key: &Key, value: &[u8]) -> Result<(), KvError> {
        let put = match cx.plant() {
            Plant::KvPutUnsynced => self.dir.publish_unsynced(cx, key.path(), Content::bytes(value), Unsynced::No).map(|_| ()),
            _ => self.dir.publish_synced(cx, key.path(), Content::bytes(value)).map(|_| ()),
        };
        put.map_err(|error| Self::refuse(KvOp::Put, Some(key), error))
    }

    /// `key` gone from the store, as durably as `durable` says; gone already is done.
    /// [`Kv::delete_unsynced`] or [`Kv::delete_synced`], chosen at run time.
    pub fn delete(&self, cx: &Cx, key: &Key, durable: Durable) -> Result<(), KvError> {
        match durable.unsynced() {
            Some(unsynced) => self.delete_unsynced(cx, key, unsynced),
            None => self.delete_synced(cx, key),
        }
    }

    /// `key` gone from the store, with nothing synced: a crash may bring it back.
    pub fn delete_unsynced(&self, cx: &Cx, key: &Key, unsynced: Unsynced) -> Result<(), KvError> {
        self.dir
            .remove_unsynced(cx, key.path(), unsynced)
            .map_err(|error| Self::refuse(KvOp::Delete, Some(key), error))
    }

    /// `key` gone from the store, and a crash keeps it gone. It waits for the device.
    pub fn delete_synced(&self, cx: &Cx, key: &Key) -> Result<(), KvError> {
        self.dir.remove_synced(cx, key.path()).map_err(|error| Self::refuse(KvOp::Delete, Some(key), error))
    }
}
