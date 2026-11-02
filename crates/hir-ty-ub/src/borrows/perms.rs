//! The permission state machine of one tag at one location.
//!
//! Miri: `tree_borrows/perms.rs`. `ReservedFrz { conflicted }` is two variants here, so the
//! whole state fits the 3 bits [`super::tree::LocationState`] packs it in.

use std::{cmp::Ordering, fmt};

use super::{
    foreign_access_skipping::IdempotentForeignAccess, tree::AccessRelatedness,
    wildcard::WildcardAccessLevel,
};
use crate::ids::AccessKind;

/// The activation states of a pointer. Miri: `perms.rs`, `PermissionPriv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(super) enum PermissionPriv {
    /// A shared reference to interior mutable data: allows every access, rejects nothing.
    Cell = 0,
    /// A local mutable reference not yet written to (`ReservedFrz { conflicted: false }`):
    /// allows child and foreign reads, becomes `Unique` on a child write, `Disabled` on a
    /// foreign write.
    ReservedFrz = 1,
    /// `ReservedFrz { conflicted: true }`: a foreign read happened while protected, so a child
    /// write while still protected is UB.
    ReservedFrzConflicted = 2,
    /// `ReservedFrz` for interior mutable types without a protector: also survives foreign
    /// writes.
    ReservedIM = 3,
    /// A unique pointer: allows child reads and writes; foreign reads freeze it, foreign
    /// writes disable it.
    Unique = 4,
    /// A shared pointer: allows reads; child writes are UB, foreign writes disable it.
    Frozen = 5,
    /// A dead pointer: allows foreign accesses, every child access is UB.
    Disabled = 6,
}

use PermissionPriv::*;

impl PermissionPriv {
    pub(super) const ALL: [PermissionPriv; 7] =
        [Cell, ReservedFrz, ReservedFrzConflicted, ReservedIM, Unique, Frozen, Disabled];

    pub(super) fn from_bits(bits: u8) -> PermissionPriv {
        match bits {
            0 => Cell,
            1 => ReservedFrz,
            2 => ReservedFrzConflicted,
            3 => ReservedIM,
            4 => Unique,
            5 => Frozen,
            6 => Disabled,
            _ => unreachable!("permission bits {bits} out of range"),
        }
    }

    /// Miri: `perms.rs`, `PermissionPriv::is_initial`.
    pub(super) fn is_initial(self) -> bool {
        matches!(self, ReservedFrz | Frozen | ReservedIM | Cell | Unique)
    }

    /// Miri: `perms.rs`, `PermissionPriv::compatible_with_protector`.
    pub(super) fn compatible_with_protector(self) -> bool {
        !matches!(self, ReservedIM | Cell)
    }

    /// Miri: `perms.rs`, `PermissionPriv::strongest_idempotent_foreign_access`.
    pub(super) fn strongest_idempotent_foreign_access(self, prot: bool) -> IdempotentForeignAccess {
        match self {
            Cell => IdempotentForeignAccess::Write,
            ReservedFrz if prot => IdempotentForeignAccess::None,
            ReservedFrz | ReservedFrzConflicted => IdempotentForeignAccess::Read,
            ReservedIM if prot => unreachable!("Protected ReservedIM should not exist!"),
            ReservedIM => IdempotentForeignAccess::Write,
            Unique => IdempotentForeignAccess::None,
            Frozen => IdempotentForeignAccess::Read,
            Disabled => IdempotentForeignAccess::Write,
        }
    }
}

impl PartialOrd for PermissionPriv {
    /// Miri: `perms.rs`, `impl PartialOrd for PermissionPriv`: the reflexive transitive closure
    /// of `Reserved < Reserved (conflicted) < Unique < Frozen < Disabled`, with `ReservedIM`
    /// and `ReservedFrz` incomparable and `Cell` comparable to nothing else. It is
    /// reachability by transitions (`permissionpriv_partialord_is_reachability`).
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        use Ordering::*;
        Some(match (*self, *other) {
            (a, b) if a == b => Equal,
            (ReservedIM, ReservedFrz | ReservedFrzConflicted)
            | (ReservedFrz | ReservedFrzConflicted, ReservedIM)
            | (Cell, _)
            | (_, Cell) => return None,
            (Disabled, _) => Greater,
            (_, Disabled) => Less,
            (Frozen, _) => Greater,
            (_, Frozen) => Less,
            (Unique, _) => Greater,
            (_, Unique) => Less,
            (ReservedFrz, ReservedFrzConflicted) => Less,
            (ReservedFrzConflicted, ReservedFrz) => Greater,
            (a, b) => unreachable!("{a:?} and {b:?} are equal"),
        })
    }
}

/// How each permission reacts to an access. `protected` is here because some permissions
/// behave differently when protected, not to check protectors. Miri: `perms.rs`,
/// `mod transition`.
pub(super) mod transition {
    use super::*;

    /// Miri: `transition::child_read`: UB on Disabled, a no-op on the rest.
    fn child_read(state: PermissionPriv, _protected: bool) -> Option<PermissionPriv> {
        Some(match state {
            Disabled => return None,
            readable @ (Cell | ReservedFrz | ReservedFrzConflicted | ReservedIM | Unique
            | Frozen) => readable,
        })
    }

    /// Miri: `transition::foreign_read`: keeps `Reserved` but marks it conflicted if
    /// protected; freezes `Unique` (disables it if protected).
    fn foreign_read(state: PermissionPriv, protected: bool) -> Option<PermissionPriv> {
        Some(match state {
            Cell => Cell,
            non_writeable @ (Frozen | Disabled) => non_writeable,
            ReservedFrz | ReservedFrzConflicted if protected => ReservedFrzConflicted,
            res @ (ReservedFrz | ReservedFrzConflicted | ReservedIM) => {
                // A protected `ReservedIM` cannot exist: protectors only create `ReservedFrz`.
                assert!(!protected);
                res
            }
            Unique => {
                if protected {
                    Disabled
                } else {
                    Frozen
                }
            }
        })
    }

    /// Miri: `transition::child_write`: `Reserved` becomes `Unique`; `Frozen` and `Disabled`
    /// are UB, and so is a conflicted `Reserved` while protected.
    fn child_write(state: PermissionPriv, protected: bool) -> Option<PermissionPriv> {
        Some(match state {
            Cell => Cell,
            ReservedFrzConflicted if protected => return None,
            ReservedFrz | ReservedFrzConflicted | ReservedIM | Unique => Unique,
            Frozen | Disabled => return None,
        })
    }

    /// Miri: `transition::foreign_write`: everything becomes `Disabled` except `Cell` and an
    /// unprotected `ReservedIM`.
    fn foreign_write(state: PermissionPriv, protected: bool) -> Option<PermissionPriv> {
        Some(match state {
            Cell => Cell,
            res @ ReservedIM => {
                assert!(!protected);
                res
            }
            _ => Disabled,
        })
    }

    /// Miri: `transition::perform_access`.
    pub(in super::super) fn perform_access(
        kind: AccessKind,
        rel_pos: AccessRelatedness,
        child: PermissionPriv,
        protected: bool,
    ) -> Option<PermissionPriv> {
        match (kind, rel_pos.is_foreign()) {
            (AccessKind::Write, true) => foreign_write(child, protected),
            (AccessKind::Read, true) => foreign_read(child, protected),
            (AccessKind::Write, false) => child_write(child, protected),
            (AccessKind::Read, false) => child_read(child, protected),
        }
    }
}

/// The public face of the state machine. Miri: `perms.rs`, `Permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Permission {
    pub(super) inner: PermissionPriv,
}

impl PartialOrd for Permission {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.inner.partial_cmp(&other.inner)
    }
}

/// A transition from one permission to the next. Miri: `perms.rs`, `PermTransition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PermTransition {
    pub(super) from: PermissionPriv,
    pub(super) to: PermissionPriv,
}

impl Permission {
    pub(super) fn is_initial(self) -> bool {
        self.inner.is_initial()
    }

    pub(super) fn is_disabled(self) -> bool {
        self.inner == Disabled
    }

    pub(super) fn is_frozen(self) -> bool {
        self.inner == Frozen
    }

    pub(super) fn is_cell(self) -> bool {
        self.inner == Cell
    }

    pub(super) fn is_unique(self) -> bool {
        self.inner == Unique
    }

    pub(super) fn new_unique() -> Permission {
        Permission { inner: Unique }
    }

    pub(super) fn new_reserved_frz() -> Permission {
        Permission { inner: ReservedFrz }
    }

    pub(super) fn new_reserved_im() -> Permission {
        Permission { inner: ReservedIM }
    }

    pub(super) fn new_frozen() -> Permission {
        Permission { inner: Frozen }
    }

    /// Only for the root of a new tree, at out-of-bounds positions.
    pub(super) fn new_disabled() -> Permission {
        Permission { inner: Disabled }
    }

    pub(super) fn new_cell() -> Permission {
        Permission { inner: Cell }
    }

    pub(super) fn compatible_with_protector(self) -> bool {
        self.inner.compatible_with_protector()
    }

    #[cfg(test)]
    pub(super) fn is_reserved_frz_with_conflicted(self, conflicted: bool) -> bool {
        match self.inner {
            ReservedFrz => !conflicted,
            ReservedFrzConflicted => conflicted,
            _ => false,
        }
    }

    /// The access a reborrow or a protector's end performs. `Cell` does none, as that could
    /// race with thread-safe interior mutability. Miri: `Permission::associated_access`.
    pub(super) fn associated_access(self) -> Option<AccessKind> {
        match self.inner {
            Cell => None,
            Unique => Some(AccessKind::Write),
            _ => Some(AccessKind::Read),
        }
    }

    /// Miri: `Permission::perform_access`.
    pub(super) fn perform_access(
        kind: AccessKind,
        rel_pos: AccessRelatedness,
        old_perm: Permission,
        protected: bool,
    ) -> Option<PermTransition> {
        let old_state = old_perm.inner;
        transition::perform_access(kind, rel_pos, old_state, protected)
            .map(|new_state| PermTransition { from: old_state, to: new_state })
    }

    /// Whether the GC may merge this (unprotected) parent into its only child: the parent
    /// must never cause UB on its own where the child would not. Checked by
    /// `tree_compacting_is_sound`. Miri: `Permission::can_be_replaced_by_child`.
    pub(super) fn can_be_replaced_by_child(self, child: Permission) -> bool {
        match (self.inner, child.inner) {
            (Cell, _) => true,
            (_, Cell) => false,
            (ReservedIM, _) => true,
            (_, ReservedIM) => false,
            (ReservedFrz | ReservedFrzConflicted, _) => true,
            (_, ReservedFrz | ReservedFrzConflicted) => false,
            (Unique, Unique | Frozen | Disabled) => true,
            (_, Unique) => false,
            (Frozen, Frozen | Disabled) => true,
            (_, Frozen) => false,
            (Disabled, Disabled) => true,
        }
    }

    /// The strongest foreign access this node survives unchanged. Miri:
    /// `Permission::strongest_idempotent_foreign_access`.
    pub(super) fn strongest_idempotent_foreign_access(self, prot: bool) -> IdempotentForeignAccess {
        self.inner.strongest_idempotent_foreign_access(prot)
    }

    /// The strongest child access this permission allows without UB. Miri:
    /// `Permission::strongest_allowed_local_access`.
    pub(super) fn strongest_allowed_local_access(self, protected: bool) -> WildcardAccessLevel {
        match self.inner {
            Disabled => WildcardAccessLevel::None,
            Frozen => WildcardAccessLevel::Read,
            ReservedFrzConflicted if protected => WildcardAccessLevel::Read,
            _ => WildcardAccessLevel::Write,
        }
    }

    /// Three-letter name, for the tree printer. Miri: `Permission::short_name`.
    pub(super) fn short_name(self) -> &'static str {
        match self.inner {
            Cell => "Cel ",
            ReservedFrz => "Res ",
            ReservedFrzConflicted => "ResC",
            ReservedIM => "ReIM",
            Unique => "Unq ",
            Frozen => "Frz ",
            Disabled => "Dis ",
        }
    }
}

impl PermTransition {
    /// Miri: `PermTransition::is_possible`.
    pub(super) fn is_possible(self) -> bool {
        self.from <= self.to
    }

    pub(super) fn is_noop(self) -> bool {
        self.from == self.to
    }

    /// The result of the transition, if it starts at `starting_point`.
    pub(super) fn applied(self, starting_point: Permission) -> Option<Permission> {
        (starting_point.inner == self.from).then_some(Permission { inner: self.to })
    }

    pub(super) fn produces_disabled(self) -> bool {
        self.to == Disabled
    }

    pub(super) fn endpoint(self) -> Permission {
        Permission { inner: self.to }
    }

    /// Fits "this transition corresponds to {summary}". Miri: `PermTransition::summary`.
    pub(super) fn summary(self) -> &'static str {
        assert!(self.is_possible());
        assert!(!self.is_noop());
        match (self.from, self.to) {
            (_, Unique) => "the first write to a 2-phase borrowed mutable reference",
            (_, Frozen) => "a loss of write permissions",
            (ReservedFrz, ReservedFrzConflicted) => {
                "a temporary loss of write permissions until function exit"
            }
            (Frozen, Disabled) => "a loss of read permissions",
            (_, Disabled) => "a loss of read and write permissions",
            (old, new) => unreachable!("Transition from {old:?} to {new:?} should never be possible"),
        }
    }

    /// Whether this earlier transition of a tag matters for the error `err` on it: drops
    /// changes of write permissions on read errors, and everything on strong-protector
    /// deallocations. Miri: `PermTransition::is_relevant`.
    pub(super) fn is_relevant(self, err: super::diagnostics::TransitionError) -> bool {
        use super::diagnostics::TransitionError;
        assert!(self.is_possible());
        if self.is_noop() {
            return false;
        }
        match err {
            TransitionError::ChildAccessForbidden(insufficient) => match (self.to, insufficient.inner) {
                (Frozen, Frozen) => true,
                (Unique, Frozen) => true,
                (Disabled, Disabled) => true,
                (ReservedFrzConflicted, ReservedFrzConflicted) => true,
                (ReservedFrzConflicted | Unique | Frozen, Disabled) => false,
                (ReservedFrzConflicted, Frozen) => false,
                (_, Unique) | (_, ReservedFrz) | (_, Cell) => {
                    unreachable!("this permission cannot cause an error")
                }
                (ReservedFrz | ReservedIM | Cell, _) => unreachable!("self is a noop transition"),
                (Unique | Frozen | Disabled, ReservedFrzConflicted | ReservedIM)
                | (Disabled, Frozen)
                | (ReservedFrzConflicted, ReservedIM) => {
                    unreachable!("permissions between self and err must be increasing")
                }
            },
            TransitionError::ProtectedDisabled(before_disabled) => {
                match (self.to, before_disabled.inner) {
                    (Unique, Unique) => true,
                    (Frozen, Frozen) => true,
                    (ReservedFrzConflicted, ReservedFrzConflicted) => true,
                    (Unique, Frozen) => false,
                    (ReservedFrzConflicted, _) => false,
                    (_, Disabled) => unreachable!(
                        "permission that results in Disabled should not itself be Disabled in the first place"
                    ),
                    (ReservedFrz | ReservedIM | Cell, _) => {
                        unreachable!("self is a noop transition")
                    }
                    (
                        Disabled,
                        Cell | ReservedFrz | ReservedFrzConflicted | ReservedIM | Unique | Frozen,
                    )
                    | (Frozen, Cell | ReservedFrz | ReservedFrzConflicted | ReservedIM | Unique)
                    | (Unique, Cell | ReservedFrz | ReservedFrzConflicted | ReservedIM) => {
                        unreachable!("permissions between self and err must be increasing")
                    }
                }
            }
            TransitionError::ProtectedDealloc => false,
        }
    }
}

impl fmt::Display for PermissionPriv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Cell => "Cell",
            ReservedFrz => "Reserved",
            ReservedFrzConflicted => "Reserved (conflicted)",
            ReservedIM => "Reserved (interior mutable)",
            Unique => "Unique",
            Frozen => "Frozen",
            Disabled => "Disabled",
        })
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl fmt::Display for PermTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "from {} to {}", self.from, self.to)
    }
}