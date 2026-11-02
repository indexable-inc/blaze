//! A table of stack links over a reserved region, one `AtomicU32` per index, committed as
//! indices are made.

use std::mem;

use super::{Links, NONE};
use crate::{MemError, Region, Tag, sync::atomic::AtomicU32};

pub(crate) struct LinkTable {
    region: Region,
}

impl LinkTable {
    const STRIDE: usize = mem::size_of::<AtomicU32>();

    pub(crate) fn new(count: u32, tag: Tag) -> Result<LinkTable, MemError> {
        Ok(LinkTable { region: Region::reserve(count as usize * Self::STRIDE, tag)? })
    }

    /// Makes the link of `index` exist, unlinked.
    ///
    /// # Safety
    ///
    /// `index` is new: no thread has seen it, so no other thread reaches its link.
    pub(crate) unsafe fn init(&self, index: u32) -> Result<(), MemError> {
        let offset = index as usize * Self::STRIDE;
        self.region.ensure(offset + Self::STRIDE)?;
        let link = self.region.at(offset).cast::<AtomicU32>();
        // SAFETY: the slot is committed (just ensured), aligned (a page-aligned base plus a
        // multiple of the stride) and, by the caller, reached by no other thread.
        unsafe { link.write(AtomicU32::new(NONE)) };
        Ok(())
    }
}

impl Links for LinkTable {
    #[inline]
    fn link(&self, index: u32) -> &AtomicU32 {
        let offset = index as usize * Self::STRIDE;
        debug_assert!(offset + Self::STRIDE <= self.region.committed());
        // SAFETY: every index a stack holds was made with `init` first, so its link is
        // committed and initialized; the region lives as long as `self`.
        unsafe { self.region.at(offset).cast::<AtomicU32>().as_ref() }
    }
}
