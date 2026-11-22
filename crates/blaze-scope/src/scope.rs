//! What name resolution adds to the item layer's words (blaze-surface's `kind`,
//! `module_scope` and `through`, the one home of namespaces, visibilities, bindings, a scope's
//! working form and who may name what): the read list a finished scope is valid by.

use blaze_id::ContentId;
use blaze_own::AVec;
use blaze_surface::ScopeBuilder;

use crate::read::{NameAnswer, Read};

/// What a read saw: a name's answer, or the digest of an exports class or a textual list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Saw {
    Name(NameAnswer),
    Digest(ContentId),
}

/// One read a scope was determined by, and what it saw.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReadEntry<N> {
    pub read: Read<N>,
    pub saw: Saw,
}

/// Exactly the reads of other modules' records that determined a scope, the negative ones
/// included (a name looked up and not found), sorted by read; a record is valid while every
/// one of them still sees what it saw. A member of a cycle's reads of its fellows are in it
/// like any other, so its list is the same whatever the schedule; a list cannot see a
/// derivation that goes around a cycle, which [`crate::Again`] guards when resolving again.
#[derive(Clone, Debug)]
pub struct ReadList<'a, N> {
    pub entries: AVec<'a, ReadEntry<N>>,
}

impl<N: Ord + Copy> ReadList<'_, N> {
    /// Whether every read still sees what it saw, `now` answering each as today's records do.
    pub fn holds(&self, mut now: impl FnMut(&Read<N>) -> Saw) -> bool {
        self.entries.iter().all(|e| now(&e.read) == e.saw)
    }

    /// Whether the list holds a read of `read`.
    pub fn contains(&self, read: &Read<N>) -> bool {
        self.entries.binary_search_by(|e| e.read.cmp(read)).is_ok()
    }
}

impl<N: PartialEq> PartialEq for ReadList<'_, N> {
    fn eq(&self, other: &Self) -> bool {
        self.entries[..] == other.entries[..]
    }
}

/// A finished module: its scope and the reads it is valid by.
#[derive(Clone, Debug)]
pub struct Resolved<'a, N> {
    pub scope: ScopeBuilder<'a, N>,
    pub reads: ReadList<'a, N>,
}

impl<N: PartialEq> PartialEq for Resolved<'_, N> {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope && self.reads == other.reads
    }
}
