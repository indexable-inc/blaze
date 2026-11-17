//! Where a file's text lives once it is in an image: the content store, by the file's content ID
//! (docs: /tmp/plan/vfs-image-next.md). The daemon's image names each file by its ID and size
//! and holds none of its bytes; a file of the image is paged from the store the first time it
//! is read, its size and hash checked ([`crate::store::Mapped`]).
//!
//! The VFS does no IO: the store is injected as a [`Texts`], which the daemon makes of its
//! `blaze-cas` store (blaze-cargo's `serve::image::StoreTexts`) and a test of a map
//! ([`HeldTexts`]).

use std::{alloc::Global, fmt, sync::Arc};

use blaze_format::{SchemaHash, schema};
use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::{Bytes, sync::Lock};

/// The schema of a file's text as the store keeps it: the file's bytes, under the key
/// `blaze_cas::Key::of(TEXT_SCHEMA, id, &[])` of its content ID `id` ([`crate::FILE_DOMAIN`]).
/// A text is made from nothing but itself, so its ID is its producer and it has no inputs.
pub const TEXT_SCHEMA: SchemaHash =
    schema("blaze v2 vfs file text 1: a file's bytes under its FILE_DOMAIN content id");

/// The store a VFS keeps its files' texts in, by content ID. Every method is a read of, or an
/// append to, memory the store owns; none waits on IO.
pub trait Texts: Send + Sync {
    /// The text of `id`, as the store verified it against the hash it was put with; `None` when
    /// it holds none. The VFS checks it is `id`'s before using it.
    fn get(&self, id: &ContentId) -> Result<Option<Bytes>, StoreFault>;
    /// Whether it holds a text of `id`, durable or put and not yet durable.
    fn has(&self, id: &ContentId) -> bool;
    /// Keeps `bytes`, whose content ID is `id`, under it: readable at once, durable once the
    /// store's next flush is.
    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), StoreFault>;
    /// Whether every put so far is durable.
    fn durable(&self) -> bool;
}

/// The store failed a read or a put of the text of `file`; what it said is in its own log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoreFault {
    pub file: ContentId,
    pub why: FaultWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultWhy {
    /// The value it read under the text's key failed its own check.
    Corrupt,
    /// It holds other bytes under the text's key.
    Mismatch,
}

impl fmt::Display for StoreFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = &self.file;
        match self.why {
            FaultWhy::Corrupt => write!(f, "the store's text of file {file:x} failed its check"),
            FaultWhy::Mismatch => write!(
                f,
                "the store holds other bytes under the key of file {file:x}'s text (a key is one \
                 value)"
            ),
        }
    }
}

impl std::error::Error for StoreFault {}

/// Texts held in a map, durable as soon as they are put unless [`HeldTexts::hold_back`] says
/// otherwise: the store of a test, a fuzz target or a tool run with no daemon.
pub struct HeldTexts {
    held: Lock<HashMap<ContentId, Bytes, Global>>,
    undurable: std::sync::atomic::AtomicBool,
}

impl HeldTexts {
    /// A map of no texts, shared.
    pub fn shared() -> Arc<HeldTexts> {
        // alloc: once per store made.
        Arc::new(HeldTexts::new())
    }

    pub fn new() -> HeldTexts {
        HeldTexts {
            // alloc: a test's or a tool's texts, each kept as put.
            held: Lock::new("vfs.texts.held", HashMap::new_in(Global)),
            undurable: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `true`: the puts from now on are not durable until it is called with `false`, as a
    /// store's are until its flush.
    pub fn hold_back(&self, held: bool) {
        self.undurable.store(held, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drops the text of `id`, as a store whose collector dropped it would have.
    pub fn forget(&self, id: &ContentId) -> bool {
        self.held.lock().remove(id).is_some()
    }

    /// Replaces the text of `id` with `bytes`, as a store that broke its promise would.
    pub fn replace(&self, id: &ContentId, bytes: &[u8]) {
        // alloc: the replacing bytes, a test's planted break.
        self.held.lock().insert(*id, Bytes::from(Arc::<[u8]>::from(bytes)));
    }

    /// How many texts it holds, and their bytes.
    pub fn census(&self) -> (usize, u64) {
        let held = self.held.lock();
        (held.len(), held.values().map(|it| it.len() as u64).sum())
    }
}

impl Default for HeldTexts {
    fn default() -> HeldTexts {
        HeldTexts::new()
    }
}

impl Texts for HeldTexts {
    fn get(&self, id: &ContentId) -> Result<Option<Bytes>, StoreFault> {
        // clone: one count of the text's owner, never its bytes.
        Ok(self.held.lock().get(id).cloned())
    }

    fn has(&self, id: &ContentId) -> bool {
        self.held.lock().contains_key(id)
    }

    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), StoreFault> {
        let mut held = self.held.lock();
        if let Some(have) = held.get(id) {
            return match **have == *bytes {
                true => Ok(()),
                false => Err(StoreFault { file: *id, why: FaultWhy::Mismatch }),
            };
        }
        // alloc: the text kept, once per text put (a copy: the store owns what it keeps).
        held.insert(*id, Bytes::from(Arc::<[u8]>::from(bytes)));
        Ok(())
    }

    fn durable(&self) -> bool {
        !self.undurable.load(std::sync::atomic::Ordering::Relaxed)
    }
}
