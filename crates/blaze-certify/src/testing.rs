//! What the models of the request machine share: a write-once store, the model file system's
//! clock and the base its requests are under. For tests only (`plants`).

use std::{cell::RefCell, collections::BTreeMap};

use blaze_core::Stamp;
use blaze_ids::ContentId;
use blaze_record::{Kind, Store};

use crate::certificate::{Shape, base};

/// The change-time granule of the model's file system: whole seconds, a coarse clock, so
/// every stamp's racy window is `blaze_vfs::COARSE_NS`, two granules.
pub const GRANULE_NS: u64 = 1_000_000_000;

/// The model's file of 32 bytes, changed at `clock_ns` to its granule.
pub fn stamp_at(clock_ns: u64) -> Stamp {
    let changed_ns = clock_ns / GRANULE_NS * GRANULE_NS;
    Stamp { ino: 1, size: 32, mtime_ns: changed_ns, ctime_ns: changed_ns, mode: blaze_core::S_IFREG | 0o644 }
}

/// The base of the model's requests.
pub fn the_base() -> ContentId {
    let id = |n: u8| ContentId::of("test request shape", &[n]);
    base(&Shape { plan: id(0), config: id(1), request: id(2) }, b"blaze test")
}

/// A write-once store.
pub struct Shelf {
    pub held: RefCell<BTreeMap<(Kind, ContentId), Vec<u8>>>,
    tally: blaze_record::Tally,
}

impl Shelf {
    pub fn new() -> Shelf {
        Shelf { held: RefCell::new(BTreeMap::new()), tally: blaze_record::Tally::new() }
    }
}

impl Default for Shelf {
    fn default() -> Shelf {
        Shelf::new()
    }
}

impl Store for Shelf {
    type Held = Vec<u8>;
    type Error = String;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Vec<u8>>, String> {
        Ok(self.held.borrow().get(&(kind, *key)).cloned())
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        let mut held = self.held.borrow_mut();
        match held.get(&(kind, key)) {
            Some(was) if was.as_slice() != bytes => Err(format!("other bytes are under {key:x}")),
            _ => {
                held.insert((kind, key), bytes.to_vec());
                Ok(())
            }
        }
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }

    fn tally(&self) -> &blaze_record::Tally {
        &self.tally
    }
}
