//! What name resolution adds to the item layer's words (blaze-surface's `kind` and
//! `module_scope`, the one home of namespaces, visibilities, bindings and a scope's working
//! form): who is asking ([`Asker`]) and what each visibility lets it name, and the read list
//! a finished scope is valid by.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::AVec;
use blaze_surface::{ScopeBuilder, Visibility};

use crate::read::{NameAnswer, Read};

/// Who is asking: a module, its crate and its chain of modules from itself up to its crate's
/// root (itself first). Visibility is a question about this chain.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Asker<'q> {
    pub(crate) krate: CrateItemKey,
    pub(crate) chain: &'q [ItemKey],
}

/// What a visibility means to an [`Asker`]: questions of name resolution alone, so they live
/// here and not on blaze-surface's [`Visibility`]. Called as methods (`vis.visible_from(q)`)
/// and by path (`Visibility::narrower(a, b, q)`).
pub(crate) trait SeenFrom: Sized {
    /// Whether a module with the chain `from` may name a binding of this visibility.
    fn visible_from(self, from: Asker<'_>) -> bool;

    /// The narrower of `self` and `b`, both seen from a module with the chain `from`: what a
    /// re-export of a `b` binding through a `self` import may be named by. A visibility `from`
    /// cannot see is the narrower one, so the result is never wider than what `from` sees.
    fn narrower(self, b: Self, from: Asker<'_>) -> Self;

    /// The wider of two visibilities that `from` both sees.
    fn wider(self, b: Self, from: Asker<'_>) -> Self;
}

impl SeenFrom for Visibility {
    fn visible_from(self, from: Asker<'_>) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Crate(krate) => krate == from.krate,
            Visibility::Restricted(module) => from.chain.contains(&module),
        }
    }

    fn narrower(self, b: Visibility, from: Asker<'_>) -> Visibility {
        narrower(self, b, from)
    }

    fn wider(self, b: Visibility, from: Asker<'_>) -> Visibility {
        if narrower(self, b, from) == self { b } else { self }
    }
}

fn narrower(a: Visibility, b: Visibility, from: Asker<'_>) -> Visibility {
    match (a, b) {
        (Visibility::Public, other) | (other, Visibility::Public) => other,
        (Visibility::Crate(_), other @ Visibility::Restricted(_))
        | (other @ Visibility::Restricted(_), Visibility::Crate(_)) => other,
        (Visibility::Crate(x), Visibility::Crate(y)) => {
            if x == from.krate {
                Visibility::Crate(y)
            } else {
                Visibility::Crate(x)
            }
        }
        (Visibility::Restricted(x), Visibility::Restricted(y)) => {
            // The deeper of two ancestors is the narrower; one `from` cannot see is narrowest.
            match (from.chain.iter().position(|&m| m == x), from.chain.iter().position(|&m| m == y)) {
                (Some(i), Some(j)) => {
                    if i <= j {
                        a
                    } else {
                        b
                    }
                }
                (None, _) => a,
                (_, None) => b,
            }
        }
    }
}

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
/// one of them still sees what it saw. Reads of a fellow of a cycle are not in it: the
/// members are written together and are valid together.
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
