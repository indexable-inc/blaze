//! Miri: `tree_borrows/perms.rs`, `mod propagation_optimization_checks`, and
//! `foreign_access_skipping.rs`, `test_order`.

use blaze_hash::HashSet;

use crate::{
    borrows::{
        exhaustive::{Exhaustive, precondition},
        foreign_access_skipping::IdempotentForeignAccess,
        perms::{Permission, PermissionPriv, transition},
        tree::AccessRelatedness,
        wildcard::WildcardAccessLevel,
    },
    ids::AccessKind,
};

impl Exhaustive for PermissionPriv {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(PermissionPriv::ALL.into_iter())
    }
}

impl Exhaustive for Permission {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new(PermissionPriv::exhaustive().map(|inner| Permission { inner }))
    }
}

impl Exhaustive for AccessKind {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new([AccessKind::Read, AccessKind::Write].into_iter())
    }
}

impl Exhaustive for AccessRelatedness {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        Box::new([AccessRelatedness::ForeignAccess, AccessRelatedness::LocalAccess].into_iter())
    }
}

#[test]
fn test_order() {
    assert!(IdempotentForeignAccess::None < IdempotentForeignAccess::Read);
    assert!(IdempotentForeignAccess::Read < IdempotentForeignAccess::Write);
}

/// `PermissionPriv::ALL` is every value once, and `from_bits` inverts `as u8`.
#[test]
fn all_permissions_listed() {
    for (i, p) in PermissionPriv::ALL.into_iter().enumerate() {
        assert_eq!(p as u8 as usize, i);
        assert_eq!(PermissionPriv::from_bits(p as u8), p);
    }
}

/// For any access, doing it twice makes the second a no-op, even if the protector is gone.
#[test]
fn all_transitions_idempotent() {
    for old in PermissionPriv::exhaustive() {
        for (old_protected, new_protected) in <(bool, bool)>::exhaustive() {
            // A protector can't appear out of nowhere.
            precondition!(old_protected || !new_protected);
            if old_protected {
                precondition!(old.compatible_with_protector());
            }
            for (access, rel_pos) in <(AccessKind, AccessRelatedness)>::exhaustive() {
                if let Some(new) = transition::perform_access(access, rel_pos, old, old_protected) {
                    assert_eq!(
                        new,
                        transition::perform_access(access, rel_pos, new, new_protected).unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn foreign_read_is_noop_after_foreign_write() {
    let old_access = AccessKind::Write;
    let new_access = AccessKind::Read;
    for old in PermissionPriv::exhaustive() {
        for [old_protected, new_protected] in <[bool; 2]>::exhaustive() {
            precondition!(old_protected || !new_protected);
            if old_protected {
                precondition!(old.compatible_with_protector());
            }
            for rel_pos in AccessRelatedness::exhaustive() {
                precondition!(rel_pos.is_foreign());
                if let Some(new) = transition::perform_access(old_access, rel_pos, old, old_protected) {
                    assert_eq!(
                        new,
                        transition::perform_access(new_access, rel_pos, new, new_protected).unwrap()
                    );
                }
            }
        }
    }
}

/// `strongest_idempotent_foreign_access` is idempotent and never UB, and a protector only
/// makes it weaker.
#[test]
fn permission_sifa_is_correct() {
    for perm in PermissionPriv::exhaustive() {
        if perm.compatible_with_protector() {
            assert!(
                perm.strongest_idempotent_foreign_access(true)
                    <= perm.strongest_idempotent_foreign_access(false)
            );
        }
        for prot in bool::exhaustive() {
            if prot {
                precondition!(perm.compatible_with_protector());
            }
            let access = perm.strongest_idempotent_foreign_access(prot);
            if access >= IdempotentForeignAccess::Read {
                assert_eq!(
                    perm,
                    transition::perform_access(
                        AccessKind::Read,
                        AccessRelatedness::ForeignAccess,
                        perm,
                        prot
                    )
                    .unwrap()
                );
            }
            if access >= IdempotentForeignAccess::Write {
                assert_eq!(
                    perm,
                    transition::perform_access(
                        AccessKind::Write,
                        AccessRelatedness::ForeignAccess,
                        perm,
                        prot
                    )
                    .unwrap()
                );
            }
        }
    }
}

/// The order on `PermissionPriv` is reachability by transitions.
#[test]
fn permissionpriv_partialord_is_reachability() {
    let mut reach = HashSet::default();
    for start in PermissionPriv::exhaustive() {
        reach.insert((start, start));
        for (access, rel) in <(AccessKind, AccessRelatedness)>::exhaustive() {
            for prot in bool::exhaustive() {
                if prot {
                    precondition!(start.compatible_with_protector());
                }
                if let Some(end) = transition::perform_access(access, rel, start, prot) {
                    reach.insert((start, end));
                }
            }
        }
    }
    let mut finished = false;
    while !finished {
        finished = true;
        for [start, mid, end] in <[PermissionPriv; 3]>::exhaustive() {
            if reach.contains(&(start, mid))
                && reach.contains(&(mid, end))
                && !reach.contains(&(start, end))
            {
                finished = false;
                reach.insert((start, end));
            }
        }
    }
    for [p1, p2] in <[PermissionPriv; 2]>::exhaustive() {
        let le12 = p1 <= p2;
        let reach12 = reach.contains(&(p1, p2));
        assert!(le12 == reach12, "`{p1} reach {p2}` ({reach12}) does not match `{p1} <= {p2}` ({le12})");
    }
}

/// `strongest_allowed_local_access` is exactly which child accesses are possible.
#[test]
fn strongest_allowed_local_access() {
    for (permission, protected) in <(Permission, bool)>::exhaustive() {
        let strongest = permission.strongest_allowed_local_access(protected);
        let read = Permission::perform_access(
            AccessKind::Read,
            AccessRelatedness::LocalAccess,
            permission,
            protected,
        )
        .is_some();
        let write = Permission::perform_access(
            AccessKind::Write,
            AccessRelatedness::LocalAccess,
            permission,
            protected,
        )
        .is_some();
        assert_eq!(read, strongest >= WildcardAccessLevel::Read);
        assert_eq!(write, strongest >= WildcardAccessLevel::Write);
    }
}
