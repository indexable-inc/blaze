//! The reserved tags' registry: one value each, apart from the kinds', and the reach walk's
//! answers.

use super::{RESERVED_FROM, ReservedTag, TagReach, reach_of};
use crate::kind::{TAGS, Tag};

/// Invariant: every reserved tag has its own value, in the reserved block, and maps back
/// to itself; no kind's tag is a reservation. (Also a compile-time assertion in reserved.rs:
/// this is its run-time statement, over the same table.)
#[test]
fn every_reserved_tag_is_unique_and_apart_from_the_kinds() {
    let all = ReservedTag::ALL;
    for (i, one) in all.iter().enumerate() {
        assert!(one.tag().0 >= RESERVED_FROM, "{} below the reserved block", one.name());
        assert_eq!(ReservedTag::of(one.tag()), Some(*one), "{} does not map back", one.name());
        assert!(one.is(one.tag()));
        for other in &all[i + 1..] {
            assert_ne!(one.tag(), other.tag(), "{} and {} share a value", one.name(), other.name());
            assert!(!one.is(other.tag()));
        }
    }
    for (tag, name) in TAGS {
        assert_eq!(ReservedTag::of(tag), None, "the kind {name} takes a reserved tag");
        assert_eq!(reach_of(tag), TagReach::Opaque, "the kind {name} is read by the reach walk");
    }
}

/// The bug this registry fixed: the expansion records' tag was `u16::MAX - 6`, the index's,
/// so the reach walk would follow an expansion record named where an index belongs. Now it
/// has its own value and is opaque to the walk; the index is the one record followed there.
#[test]
fn an_expansion_is_not_an_index() {
    assert_ne!(ReservedTag::Expansion.tag(), ReservedTag::Index.tag());
    assert_ne!(ReservedTag::Expansion.tag(), Tag(u16::MAX - 6), "the expansion's tag is the one it shared with the index");
    assert_eq!(reach_of(ReservedTag::Expansion.tag()), TagReach::Opaque);
    assert!(!ReservedTag::Expansion.follows_keys());
    assert_eq!(reach_of(ReservedTag::Index.tag()), TagReach::IndexKeys);
    let followed: Vec<ReservedTag> = ReservedTag::ALL.into_iter().filter(|it| it.reach() == TagReach::IndexKeys).collect();
    assert_eq!(followed, [ReservedTag::Index]);
    let keys: Vec<ReservedTag> = ReservedTag::ALL.into_iter().filter(|it| it.follows_keys()).collect();
    assert_eq!(keys, [ReservedTag::Root, ReservedTag::Index, ReservedTag::IndexedRoot]);
}

/// An unreserved value in the block, and the block's edges, are no reservation.
#[test]
fn an_unreserved_value_is_none() {
    assert_eq!(ReservedTag::of(Tag(RESERVED_FROM)), None);
    assert_eq!(ReservedTag::of(Tag(RESERVED_FROM - 1)), None);
    assert_eq!(ReservedTag::of(Tag(0)), None);
    assert_eq!(reach_of(Tag(RESERVED_FROM)), TagReach::Opaque);
}
