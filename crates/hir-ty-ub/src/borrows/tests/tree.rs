//! Miri: `tree_borrows/tree/tests.rs`: exhaustive checks of the per-location state machine
//! (read reordering, GC compaction, `noalias`, spurious reads), plus the check that the packed
//! `LocationState` holds exactly the fields Miri's has.

use std::fmt;

use blaze_hash::HashSet;

use crate::{
    borrows::{
        exhaustive::{Exhaustive, Never, precondition},
        foreign_access_skipping::IdempotentForeignAccess,
        perms::{Permission, PermissionPriv},
        tree::{AccessRelatedness, LocationState},
    },
    ids::AccessKind,
};

impl Exhaustive for LocationState {
    fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
        // The SIFA stays `None`, as in Miri: it is a cache.
        Box::new(
            <(Permission, bool)>::exhaustive()
                .map(|(permission, accessed)| {
                    LocationState::pack(permission, accessed, IdempotentForeignAccess::default())
                })
                .filter(|x| x.possible()),
        )
    }
}

fn compatible_with_protector(state: LocationState) -> bool {
    state.permission().compatible_with_protector()
}

/// Every combination of the three fields packs into a distinct byte and unpacks to itself.
#[test]
fn packed_state_roundtrip() {
    let sifas = [IdempotentForeignAccess::None, IdempotentForeignAccess::Read, IdempotentForeignAccess::Write];
    let mut seen = HashSet::default();
    for inner in PermissionPriv::ALL {
        for accessed in bool::exhaustive() {
            for sifa in sifas {
                let permission = Permission { inner };
                let mut s = LocationState::pack(permission, accessed, sifa);
                assert_eq!(s.permission(), permission);
                assert_eq!(s.accessed(), accessed);
                assert_eq!(s.idempotent_foreign_access(), sifa);
                assert!(seen.insert(s));
                for other in sifas {
                    s.set_idempotent_foreign_access(other);
                    assert_eq!((s.permission(), s.accessed()), (permission, accessed));
                    assert_eq!(s.idempotent_foreign_access(), other);
                }
            }
        }
    }
}

/// For any state and two reads `r1`, `r2`: if `loc + r1 + r2` is not UB, `loc + r2 + r1` is
/// not either, and ends in the same state, so reads may be reordered.
#[test]
fn all_read_accesses_commute() {
    let kind = AccessKind::Read;
    for [rel1, rel2] in <[AccessRelatedness; 2]>::exhaustive() {
        for protected in bool::exhaustive() {
            for loc in LocationState::exhaustive() {
                if protected {
                    precondition!(compatible_with_protector(loc));
                }
                let mut loc12 = loc;
                precondition!(loc12.perform_access(kind, rel1, protected).is_ok());
                precondition!(loc12.perform_access(kind, rel2, protected).is_ok());

                let mut loc21 = loc;
                loc21.perform_access(kind, rel2, protected).unwrap();
                loc21.perform_access(kind, rel1, protected).unwrap();

                assert_eq!(loc12, loc21, "Read accesses {rel1:?} followed by {rel2:?} do not commute !");
            }
        }
    }
}

fn as_foreign_or_child(related: AccessRelatedness) -> &'static str {
    if related.is_foreign() { "foreign" } else { "child" }
}

fn as_protected(b: bool) -> &'static str {
    if b { " (protected)" } else { "" }
}

fn as_lazy_or_accessed(b: bool) -> &'static str {
    if b { "accessed" } else { "lazy" }
}

/// The GC replaces an unprotected parent by its child when `can_be_replaced_by_child` holds;
/// that must be a simulation, and UB at the parent must also be UB at the child.
#[test]
fn tree_compacting_is_sound() {
    let parent_protected = false;
    for ([parent, child], child_protected) in <([LocationState; 2], bool)>::exhaustive() {
        if child_protected {
            precondition!(compatible_with_protector(child))
        }
        precondition!(parent.permission().can_be_replaced_by_child(child.permission()));
        for (kind, rel) in <(AccessKind, AccessRelatedness)>::exhaustive() {
            let new_parent = parent.perform_access_no_fluff(kind, rel, parent_protected);
            let new_child = child.perform_access_no_fluff(kind, rel, child_protected);
            match (new_parent, new_child) {
                (Some(np), Some(nc)) => {
                    assert!(
                        np.permission().can_be_replaced_by_child(nc.permission()),
                        "`can_be_replaced_by_child` is not a simulation: on a {} {:?} to a {} parent and a {} {}{} child, the parent becomes {}, the child becomes {}, and these are not in simulation!",
                        as_foreign_or_child(rel),
                        kind,
                        parent.permission(),
                        as_lazy_or_accessed(child.accessed()),
                        child.permission(),
                        as_protected(child_protected),
                        np.permission(),
                        nc.permission()
                    )
                }
                (_, None) => {}
                (None, Some(nc)) => panic!(
                    "`can_be_replaced_by_child` does not have the UB property: on a {} {:?} to a(n) {} parent and a(n) {} {}{} child, only the parent causes UB, while the child becomes {}, and it is not allowed for only the parent to cause UB!",
                    as_foreign_or_child(rel),
                    kind,
                    parent.permission(),
                    as_lazy_or_accessed(child.accessed()),
                    child.permission(),
                    as_protected(child_protected),
                    nc.permission()
                ),
            }
        }
    }
}

/// Of a foreign and a child access to a protected tag, both succeed only if both are reads.
#[test]
fn protected_enforces_noalias() {
    for [rel1, rel2] in <[AccessRelatedness; 2]>::exhaustive() {
        precondition!(rel1.is_foreign() != rel2.is_foreign());
        for [kind1, kind2] in <[AccessKind; 2]>::exhaustive() {
            let protected = true;
            for mut state in LocationState::exhaustive().filter(|s| compatible_with_protector(*s)) {
                precondition!(state.perform_access(kind1, rel1, protected).is_ok());
                precondition!(state.perform_access(kind2, rel2, protected).is_ok());
                assert!(
                    kind1 == AccessKind::Read && kind2 == AccessKind::Read,
                    "failed to enforce noalias between two accesses that are not both reads"
                );
            }
        }
    }
}

/// Inserting a spurious read through a protected `x` (reborrowed, no children) never adds UB
/// to code that had none, whatever the code does through `x`, another pointer `y` or a third
/// one, including retagging `y` and ending `y`'s protector:
///
/// ```text
///     [thread 1]             || [thread 2]
///                            || y exists
///     retag x (protect)      ||
///                      arbitrary code
///     <spurious read x>      ||
///                      arbitrary code (may also end x's protector)
/// ```
mod spurious_read {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PtrSelector {
        X,
        Y,
        Other,
    }

    /// `y` is never a child of `x`: `x` was just retagged.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum RelPosXY {
        MutuallyForeign,
        XChildY,
    }

    impl Exhaustive for PtrSelector {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            Box::new([PtrSelector::X, PtrSelector::Y, PtrSelector::Other].into_iter())
        }
    }

    impl fmt::Display for PtrSelector {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                PtrSelector::X => "x",
                PtrSelector::Y => "y",
                PtrSelector::Other => "z",
            })
        }
    }

    impl Exhaustive for RelPosXY {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            Box::new([RelPosXY::MutuallyForeign, RelPosXY::XChildY].into_iter())
        }
    }

    impl fmt::Display for RelPosXY {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                RelPosXY::MutuallyForeign => "x and y are mutually foreign",
                RelPosXY::XChildY => "x is a child of y",
            })
        }
    }

    impl PtrSelector {
        /// Where the accessed pointer is relative to `x` and to `y`, up to `is_foreign`.
        fn rel_pair(self, xy_rel: RelPosXY) -> (AccessRelatedness, AccessRelatedness) {
            use AccessRelatedness::*;
            match xy_rel {
                RelPosXY::MutuallyForeign => match self {
                    PtrSelector::X => (LocalAccess, ForeignAccess),
                    PtrSelector::Y => (ForeignAccess, LocalAccess),
                    PtrSelector::Other => (ForeignAccess, ForeignAccess),
                },
                RelPosXY::XChildY => match self {
                    PtrSelector::X => (LocalAccess, LocalAccess),
                    PtrSelector::Y => (ForeignAccess, LocalAccess),
                    PtrSelector::Other => (ForeignAccess, ForeignAccess),
                },
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct TestAccess {
        ptr: PtrSelector,
        kind: AccessKind,
    }

    impl Exhaustive for TestAccess {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            Box::new(<(PtrSelector, AccessKind)>::exhaustive().map(|(ptr, kind)| TestAccess { ptr, kind }))
        }
    }

    impl fmt::Display for TestAccess {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let kind_text = match self.kind {
                AccessKind::Read => "read",
                AccessKind::Write => "write",
            };
            write!(f, "{kind_text} {}", self.ptr)
        }
    }

    type AllowRet = ();
    type NoRet = Never;

    /// The events that matter to two pointers: an access to the location, the end of either
    /// one's protector, a retag of `y` (to an initial state, making it foreign to `x`). An
    /// uninhabited `RetX`/`RetY` forbids that pointer's protector to end.
    #[derive(Clone)]
    enum TestEvent<RetX, RetY> {
        Access(TestAccess),
        RetX(RetX),
        RetY(RetY),
        RetagY(LocStateProt),
    }

    impl<RetX, RetY> Exhaustive for TestEvent<RetX, RetY>
    where
        RetX: Exhaustive + 'static,
        RetY: Exhaustive + 'static,
    {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            Box::new(
                <TestAccess>::exhaustive()
                    .map(TestEvent::Access)
                    .chain(RetX::exhaustive().map(TestEvent::RetX))
                    .chain(RetY::exhaustive().map(TestEvent::RetY))
                    .chain(LocStateProt::exhaustive().filter_map(|s| s.is_initial().then_some(TestEvent::RetagY(s)))),
            )
        }
    }

    impl<RetX, RetY> fmt::Display for TestEvent<RetX, RetY> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TestEvent::Access(acc) => write!(f, "{acc}"),
                TestEvent::RetX(_) => write!(f, "ret x"),
                TestEvent::RetY(_) => write!(f, "ret y"),
                TestEvent::RetagY(newp) => write!(f, "retag y ({newp})"),
            }
        }
    }

    /// One pointer's state at the location, with its protector.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct LocStateProt {
        state: LocationState,
        prot: bool,
    }

    impl LocStateProt {
        fn is_initial(&self) -> bool {
            self.state.permission().is_initial()
        }

        fn perform_access(&self, kind: AccessKind, rel: AccessRelatedness) -> Result<Self, ()> {
            let mut state = self.state;
            state.perform_access(kind, rel, self.prot).map_err(|_| ())?;
            Ok(LocStateProt { state, prot: self.prot })
        }

        /// Drops the protector, without the protector-end access (see `end_protector_x`).
        fn end_protector(&self) -> Self {
            LocStateProt { prot: false, state: self.state }
        }
    }

    impl Exhaustive for LocStateProt {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            Box::new(<(LocationState, bool)>::exhaustive().map(|(state, prot)| LocStateProt { state, prot }))
        }
    }

    impl fmt::Display for LocStateProt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.state)?;
            if self.prot {
                write!(f, ", protect")?;
            }
            Ok(())
        }
    }

    /// Both pointers' states and where they are relative to each other.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct LocStateProtPair {
        xy_rel: RelPosXY,
        x: LocStateProt,
        y: LocStateProt,
    }

    impl LocStateProtPair {
        fn perform_test_access(self, acc: &TestAccess) -> Result<Self, ()> {
            let (xrel, yrel) = acc.ptr.rel_pair(self.xy_rel);
            let x = self.x.perform_access(acc.kind, xrel)?;
            let y = self.y.perform_access(acc.kind, yrel)?;
            Ok(LocStateProtPair { x, y, ..self })
        }

        /// The access a retag or a protector's end makes through `ptr` if it is accessed.
        fn retag_dependent_access(self, ptr: PtrSelector) -> Result<Self, ()> {
            let accessed = match ptr {
                PtrSelector::X => self.x.state.permission().associated_access().filter(|_| self.x.state.accessed()),
                PtrSelector::Y => self.y.state.permission().associated_access().filter(|_| self.y.state.accessed()),
                PtrSelector::Other => panic!("the `accessed` status of `PtrSelector::Other` is unknown"),
            };
            match accessed {
                Some(kind) => self.perform_test_access(&TestAccess { ptr, kind }),
                None => Ok(self),
            }
        }

        fn end_protector_x(self) -> Result<Self, ()> {
            let x = self.x.end_protector();
            LocStateProtPair { x, ..self }.retag_dependent_access(PtrSelector::X)
        }

        fn end_protector_y(self) -> Result<Self, ()> {
            let y = self.y.end_protector();
            LocStateProtPair { y, ..self }.retag_dependent_access(PtrSelector::Y)
        }

        fn retag_y(self, new_y: LocStateProt) -> Result<Self, ()> {
            assert!(new_y.is_initial());
            if new_y.prot && !compatible_with_protector(new_y.state) {
                return Err(());
            }
            LocStateProtPair { y: new_y, xy_rel: RelPosXY::MutuallyForeign, ..self }
                .retag_dependent_access(PtrSelector::Y)
        }

        fn perform_test_event<RetX, RetY>(self, evt: &TestEvent<RetX, RetY>) -> Result<Self, ()> {
            match evt {
                TestEvent::Access(acc) => self.perform_test_access(acc),
                TestEvent::RetX(_) => self.end_protector_x(),
                TestEvent::RetY(_) => self.end_protector_y(),
                TestEvent::RetagY(newp) => self.retag_y(newp.clone()),
            }
        }

        /// Every state reachable from `self` without UB, each once, with a sequence of events
        /// reaching it.
        fn all_states_reachable_via_opaque_code<RetX, RetY>(self) -> impl Iterator<Item = (Self, OpaqueCode<RetX, RetY>)>
        where
            RetX: Exhaustive + Clone + 'static,
            RetY: Exhaustive + Clone + 'static,
        {
            let mut handle: Vec<(Self, OpaqueCode<_, _>)> = vec![(self, OpaqueCode { events: Vec::new() })];
            let mut paths: Vec<(Self, OpaqueCode<_, _>)> = Vec::new();
            let mut seen: HashSet<Self> = HashSet::default();
            while let Some((state, path)) = handle.pop() {
                for evt in <TestEvent<RetX, RetY>>::exhaustive() {
                    if let Ok(next) = state.clone().perform_test_event(&evt)
                        && seen.insert(next.clone())
                    {
                        let mut evts = path.clone();
                        evts.events.push(evt);
                        paths.push((next.clone(), evts.clone()));
                        handle.push((next, evts));
                    }
                }
            }
            paths.into_iter()
        }

        /// Whether some sequence of events is UB from `other` (the target) but not from `self`
        /// (the source).
        fn distinguishable<RetX, RetY>(&self, other: &Self) -> bool
        where
            RetX: Exhaustive + 'static,
            RetY: Exhaustive + 'static,
        {
            if self == other {
                return false;
            }
            let mut states = vec![(self.clone(), other.clone())];
            let mut seen = HashSet::default();
            while let Some(state) = states.pop() {
                if !seen.insert(state.clone()) {
                    continue;
                }
                let (source, target) = state;
                for evt in <TestEvent<RetX, RetY>>::exhaustive() {
                    let Ok(new_source) = source.clone().perform_test_event(&evt) else { continue };
                    let Ok(new_target) = target.clone().perform_test_event(&evt) else { return true };
                    if new_source == new_target {
                        continue;
                    }
                    states.push((new_source, new_target));
                }
            }
            false
        }
    }

    impl fmt::Display for LocStateProtPair {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "x:{}, y:{}", self.x, self.y)
        }
    }

    #[derive(Clone)]
    struct OpaqueCode<RetX, RetY> {
        events: Vec<TestEvent<RetX, RetY>>,
    }

    impl<RetX, RetY> fmt::Display for OpaqueCode<RetX, RetY> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for evt in &self.events {
                write!(f, "{evt}; ")?;
            }
            Ok(())
        }
    }

    /// `Reserved` and `Reserved (conflicted)` can't be told apart where spurious reads are
    /// inserted.
    #[test]
    fn reserved_aliased_protected_indistinguishable() {
        let source = LocStateProtPair {
            xy_rel: RelPosXY::MutuallyForeign,
            x: LocStateProt {
                state: LocationState::new_accessed(Permission::new_frozen(), IdempotentForeignAccess::default()),
                prot: true,
            },
            y: LocStateProt {
                state: LocationState::new_non_accessed(
                    Permission::new_reserved_frz(),
                    IdempotentForeignAccess::default(),
                ),
                prot: true,
            },
        };
        let acc = TestAccess { ptr: PtrSelector::X, kind: AccessKind::Read };
        let target = source.clone().perform_test_access(&acc).unwrap();
        assert!(source.y.state.permission().is_reserved_frz_with_conflicted(false));
        assert!(target.y.state.permission().is_reserved_frz_with_conflicted(true));
        assert!(!source.distinguishable::<(), ()>(&target))
    }

    #[derive(Clone, Debug)]
    struct Pattern {
        /// Where `x` and `y` start relative to each other (a `retag y` changes it).
        xy_rel: RelPosXY,
        /// What `x` is retagged to: initial, accessed, protected.
        x_retag_perm: LocationState,
        /// What `y` is at the start: any state.
        y_current_perm: LocationState,
        y_protected: bool,
    }

    impl Exhaustive for Pattern {
        fn exhaustive() -> Box<dyn Iterator<Item = Self>> {
            let mut v = Vec::new();
            for xy_rel in RelPosXY::exhaustive() {
                for (x_retag_perm, y_current_perm) in <(LocationState, LocationState)>::exhaustive() {
                    precondition!(x_retag_perm.accessed());
                    precondition!(x_retag_perm.permission().is_initial());
                    precondition!(compatible_with_protector(x_retag_perm));
                    for y_protected in bool::exhaustive() {
                        if y_protected {
                            precondition!(compatible_with_protector(y_current_perm));
                        }
                        v.push(Pattern { xy_rel, x_retag_perm, y_current_perm, y_protected });
                    }
                }
            }
            Box::new(v.into_iter())
        }
    }

    impl fmt::Display for Pattern {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let (x, y) = self.retag_permissions();
            write!(f, "{}; ", self.xy_rel)?;
            write!(f, "y: ({y}); ")?;
            write!(f, "retag x ({x}); ")?;
            write!(f, "<arbitrary code>; <spurious read x>;")
        }
    }

    impl Pattern {
        fn retag_permissions(&self) -> (LocStateProt, LocStateProt) {
            let x = LocStateProt { state: self.x_retag_perm, prot: true };
            let y = LocStateProt { state: self.y_current_perm, prot: self.y_protected };
            (x, y)
        }

        /// The state right after the retag of `x`, including its access.
        fn initial_state(&self) -> Result<LocStateProtPair, ()> {
            let (x, y) = self.retag_permissions();
            LocStateProtPair { xy_rel: self.xy_rel, x, y }.retag_dependent_access(PtrSelector::X)
        }
    }

    /// Runs each pattern as is and with a spurious read inserted, and reports UB only the
    /// second has.
    #[test]
    fn test_all_patterns() {
        let mut ok = 0;
        let mut err = 0;
        for pat in Pattern::exhaustive() {
            let Ok(initial_source) = pat.initial_state() else { continue };
            for (final_source, opaque) in initial_source.all_states_reachable_via_opaque_code::<NoRet, AllowRet>() {
                let distinguishable = {
                    assert!(final_source.x.prot);
                    let spurious_read = TestAccess { ptr: PtrSelector::X, kind: AccessKind::Read };
                    match final_source.clone().perform_test_access(&spurious_read) {
                        Ok(final_target) => final_source
                            .distinguishable::<AllowRet, AllowRet>(&final_target)
                            .then(|| final_target.to_string()),
                        Err(()) => Some("UB".to_owned()),
                    }
                };
                if let Some(final_target) = distinguishable {
                    eprintln!(
                        "For pattern '{pat}', inserting a spurious read through x makes the final state '{final_target}' instead of '{final_source}' which is observable"
                    );
                    eprintln!("  (arbitrary code instanciated with '{opaque}')");
                    err += 1;
                    break;
                }
                ok += 1;
            }
        }
        assert!(err == 0, "Test failed after {err}/{} patterns had UB in the target but not the source", ok + err);
    }
}
