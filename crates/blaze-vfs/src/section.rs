//! A section of the daemon's mapped image (docs/v2/plan/design/restore.md), held for as long as
//! a view of it lives: the VFS's tree and its watchers' metadata are read from one in place.

use std::{fmt, ops::Range};

use blaze_core::Mapping;

/// Bytes of a mapped file the image has verified (its header's hash, each page group's CRC):
/// only verified bytes are made into one.
#[derive(Clone)]
pub struct Section {
    map: Mapping,
    start: usize,
    end: usize,
}

impl Section {
    /// `range` of `map`; `None` when it is not inside it.
    pub fn new(map: Mapping, range: Range<usize>) -> Option<Section> {
        (range.start <= range.end && range.end <= map.bytes().len())
            .then_some(Section { map, start: range.start, end: range.end })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.map.bytes()[self.start..self.end]
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// `range` of this section, sharing its mapping; `None` when it is not inside it.
    pub fn slice(&self, range: Range<usize>) -> Option<Section> {
        (range.start <= range.end && range.end <= self.len()).then(|| Section {
            // clone: an `Arc` of the mapping, once per sub-section.
            map: self.map.clone(),
            start: self.start + range.start,
            end: self.start + range.end,
        })
    }
}

impl fmt::Debug for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Section({}..{})", self.start, self.end)
    }
}
