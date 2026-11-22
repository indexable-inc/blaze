//! A workspace's own files, read from a folder on disk as the names library asks for them (any
//! thread): the one reader of the three that does IO. Each file is read once and held, whole,
//! until the checkout is dropped.

use std::boxed::Box as StdBox;
use std::path::PathBuf;
use std::vec::Vec as StdVec;

use blaze_core::lock::Lock;

use crate::Tree;

/// The checkout at `root`: a path asked for is joined to it.
pub struct Checkout {
    root: PathBuf,
    /// Every file read, whole; a block is never moved or freed while the checkout lives.
    held: Lock<StdVec<StdBox<[u8]>>>,
}

impl Checkout {
    pub fn new(root: PathBuf) -> Checkout {
        Checkout { root, held: Lock::new("world.checkout.files", StdVec::new()) }
    }

    /// How many files were read, and their bytes.
    pub fn read_so_far(&self) -> (usize, usize) {
        let held = self.held.lock();
        (held.len(), held.iter().map(|b| b.len()).sum())
    }
}

impl Tree for Checkout {
    fn file(&self, path: &str) -> Option<&[u8]> {
        // The names library hands paths without `.` or `..`; refuse one that has them rather
        // than leave the root.
        if path.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let bytes: StdBox<[u8]> = std::fs::read(self.root.join(path)).ok()?.into_boxed_slice();
        let mut held = self.held.lock();
        held.push(bytes);
        let block: &[u8] = held.last().expect("pushed above");
        // SAFETY: the block is a `Box<[u8]>` held in `self.held` until `self` is dropped and
        // never removed or written; a push may move the `Box` pointers but not the blocks they
        // own, so the bytes stay at this address for the borrow of `self`.
        Some(unsafe { core::slice::from_raw_parts(block.as_ptr(), block.len()) })
    }
}
