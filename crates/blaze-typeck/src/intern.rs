//! The index of interned lists: a list's content cannot be a map key without a second copy of
//! it, so the map is from the content's hash to the newest list with it, and each list links to
//! the previous one with the same hash. One `blaze_hash` table (rule 30); a lookup compares
//! content only among lists of equal hash.

use blaze_hash::ByHash;
use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit};

const NONE: u32 = u32::MAX;

/// Lists by content hash. Lists are numbered densely in the order inserted.
pub(crate) struct ListIndex<'a> {
    newest: ByHash<u64, u32, Alloc<'a>>,
    /// Per list: the previous list with the same hash, or `NONE`.
    prev: Vec<u32, Alloc<'a>>,
}

impl<'a> ListIndex<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> ListIndex<'a> {
        ListIndex { newest: ByHash::new_in(alloc), prev: Vec::new_in(alloc) }
    }

    /// The list of `hash` for which `eq` holds. The chain is at most every list.
    pub(crate) fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        let mut at = *self.newest.get(&hash)?;
        for _ in 0..=self.prev.len() {
            if at == NONE {
                return None;
            }
            if eq(at) {
                return Some(at);
            }
            at = self.prev[at as usize];
        }
        None
    }

    /// Adds the next list, numbered `self.len()`, under `hash`.
    pub(crate) fn push(&mut self, hash: u64) -> Result<u32, CheckError> {
        let id = u32::try_from(self.prev.len()).map_err(|_| CheckError::Limit(Limit::Table))?;
        if id == NONE {
            return Err(CheckError::Limit(Limit::Table));
        }
        let prev = self.newest.insert(hash, id).unwrap_or(NONE);
        self.prev.push(prev);
        Ok(id)
    }
}
