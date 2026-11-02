//! Skipping subtrees on foreign accesses: each node and location records the strongest foreign
//! access its whole subtree is idempotent under (its SIFA), and a foreign access no stronger
//! than that skips the subtree. At each location a child's SIFA is at least its parent's; a
//! retag, which inserts a node with a weaker SIFA, restores that by weakening its parents
//! (`Tree::update_idempotent_foreign_access_after_retag`).
//!
//! Miri: `tree_borrows/foreign_access_skipping.rs`.

use super::tree::AccessRelatedness;
use crate::ids::AccessKind;

/// `None < Read < Write`, weaker to stronger; the derived order is relied on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(u8)]
pub(super) enum IdempotentForeignAccess {
    #[default]
    None = 0,
    Read = 1,
    Write = 2,
}

impl IdempotentForeignAccess {
    pub(super) fn from_bits(bits: u8) -> IdempotentForeignAccess {
        match bits {
            0 => IdempotentForeignAccess::None,
            1 => IdempotentForeignAccess::Read,
            2 => IdempotentForeignAccess::Write,
            _ => unreachable!("SIFA bits {bits} out of range"),
        }
    }

    /// Miri: `IdempotentForeignAccess::can_skip_foreign_access`.
    pub(super) fn can_skip_foreign_access(self, happening_next: IdempotentForeignAccess) -> bool {
        debug_assert!(happening_next.is_foreign());
        happening_next <= self
    }

    /// Miri: `IdempotentForeignAccess::record_new`: a local access resets, a foreign one
    /// keeps or strengthens.
    pub(super) fn record_new(&mut self, just_happened: IdempotentForeignAccess) {
        if just_happened.is_local() {
            *self = IdempotentForeignAccess::None;
        } else {
            *self = just_happened.max(*self);
        }
    }

    pub(super) fn is_local(self) -> bool {
        matches!(self, IdempotentForeignAccess::None)
    }

    pub(super) fn is_foreign(self) -> bool {
        !self.is_local()
    }

    pub(super) fn from_foreign(acc: AccessKind) -> IdempotentForeignAccess {
        match acc {
            AccessKind::Read => IdempotentForeignAccess::Read,
            AccessKind::Write => IdempotentForeignAccess::Write,
        }
    }

    pub(super) fn from_acc_and_rel(
        acc: AccessKind,
        rel: AccessRelatedness,
    ) -> IdempotentForeignAccess {
        if rel.is_foreign() { Self::from_foreign(acc) } else { IdempotentForeignAccess::None }
    }

    /// Weakens to at most `strongest_allowed`; whether it changed. Miri:
    /// `IdempotentForeignAccess::ensure_no_stronger_than`.
    pub(super) fn ensure_no_stronger_than(
        &mut self,
        strongest_allowed: IdempotentForeignAccess,
    ) -> bool {
        if *self > strongest_allowed {
            *self = strongest_allowed;
            true
        } else {
            false
        }
    }
}
