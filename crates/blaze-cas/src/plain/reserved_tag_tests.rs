//! The reserved tags (blaze-keyed `ReservedTag`, the one registry) as the store reads them, on
//! the deterministic simulation (rules 23, 26, 43, 64).
//!
//! The bug the registry fixed: an expansion record was filed under `u16::MAX - 6`, the index's
//! tag, so a record whose bytes look like sorted 32-byte keys, named where an index belongs,
//! would be FOLLOWED by the reach walk and keep what its bytes happen to name. The walk now asks
//! the registry ([`reach_of`]): only a record of the index's tag is followed there; any other
//! is the store's `NotAnIndex` refusal, by name.
//!
//! One history per tag: a command puts eight target records no index names, a LOOK-ALIKE record
//! of the tag under test whose bytes are the targets' sorted keys, the owner's real index, and
//! an indexed root naming both; long dead records share the segment, so cleanup sweeps it.
//! Later commands fold slices past every lease and grace. What the history shows is one of:
//! refused (the walk, or the commit's check of what its roots name, stopped naming a record
//! that is not an index), followed (the targets survive the sweep: the look-alike kept them),
//! not followed (the targets are swept, nothing said).

use std::{alloc::Global, time::Duration};

use blaze_core::sim::{Faults, Sim};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, ReservedTag, Tag, TagReach, reach_of};
use blaze_mem::Frame;

use super::core::{GRACE_NS, LEASE_NS, Plant, ROOT_LIFE_NS};
use super::index_tests::{World, key, readable, value};
use super::test_root;
use crate::Key;

const MINUTE: Duration = Duration::from_secs(60);
/// The owner's record, named by a real index.
const OWNER: u32 = 0;
/// The records the look-alike's bytes name, and nothing else does.
const TARGETS: core::ops::Range<u32> = 1..9;
/// Dead records in the same segment, long (index_tests.rs `value`), so the segment is mostly
/// dead and the first slice that looks at it sweeps it.
const DEAD: core::ops::Range<u32> = 20_000..20_064;
/// The expansion records' tag before segment format 3: the index's.
const SHARED_BEFORE: Tag = Tag(u16::MAX - 6);

/// What a history showed of the look-alike.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Walk {
    /// Refused by name: a record named where an index belongs is not an index.
    Refused,
    /// Followed: every target survived the sweep, kept by the look-alike's bytes.
    Followed,
    /// Neither: the targets were swept and nothing was said.
    NotFollowed,
}

fn look_alike() -> Key {
    Key(ContentId::of("reserved tag test look-alike", b"e"))
}

/// The targets' keys, sorted by their bytes, concatenated: an index's payload in form.
fn targets_as_keys() -> Vec<u8> {
    let mut named: Vec<Key> = TARGETS.map(key).collect();
    named.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
    named.iter().flat_map(|it| it.0.0).collect()
}

/// Whether a command's failure is the store's refusal of a record that is not an index
/// (blaze-keyed `StoreError::NotAnIndex`).
fn not_an_index(why: &str) -> bool {
    why.contains("not an index")
}

/// One command putting `puts` and folding one slice: `true` when the store refused a record
/// named where an index belongs.
fn fold(w: &mut World<'_>, puts: &[u32]) -> Result<bool, String> {
    match w.command(puts, |_| Ok(()), true) {
        Ok(()) => Ok(false),
        Err(why) if not_an_index(&why) => Ok(true),
        Err(why) => Err(why),
    }
}

/// One history with the look-alike filed under `tag`.
fn history(tag: Tag, root: &Frame) -> Result<Walk, String> {
    let mut w = World { sim: Sim::new(67, Faults::default()), next: 0, plant: Plant::None, root };
    let mut puts: Vec<u32> = core::iter::once(OWNER).chain(TARGETS).chain(DEAD).collect();
    puts.sort_unstable();
    let first = w.command(
        &puts,
        |store| {
            store.put(look_alike(), Namespace::LOCAL, tag, &targets_as_keys()).map_err(|why| format!("the look-alike's put: {why:?}"))?;
            let own = store.index(&[key(OWNER)]).map_err(|why| format!("the owner's index: {why}"))?;
            store.root_through("answer", &[own, look_alike()], ROOT_LIFE_NS).map_err(|why| format!("the root: {why}"))?;
            Ok(())
        },
        false,
    );
    match first {
        Ok(()) => {}
        Err(why) if not_an_index(&why) => return Ok(Walk::Refused),
        Err(why) => return Err(why),
    }
    for g in 0..6u32 {
        let garbage: Vec<u32> = (30_000 + g * 32..30_000 + (g + 1) * 32).collect();
        if fold(&mut w, &garbage)? {
            return Ok(Walk::Refused);
        }
        w.sim.advance(MINUTE);
    }
    w.sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + MINUTE);
    for _ in 0..4 {
        if fold(&mut w, &[])? {
            return Ok(Walk::Refused);
        }
        w.sim.advance(Duration::from_secs(1));
    }
    let dead = readable(&mut w, DEAD)?;
    if !dead.is_empty() {
        return Err(format!("{} dead records were not swept: no slice looked at the segment", dead.len()));
    }
    let kept = readable(&mut w, TARGETS)?;
    match kept.len() {
        0 => Ok(Walk::NotFollowed),
        n if n == TARGETS.len() => Ok(Walk::Followed),
        n => Err(format!("{n} of the {} targets survived: part of a record followed", TARGETS.len())),
    }
}

/// The invariant: a record of the expansion's tag whose bytes look like sorted keys, named
/// where an index belongs, is not followed by the reach walk: the store refuses it by name.
/// Prints "reserved tags: an expansion named as an index is refused".
#[test]
fn an_expansion_record_is_not_followed_as_an_index() {
    let root = test_root();
    let walk = history(ReservedTag::Expansion.tag(), &root).expect("the history runs");
    assert_eq!(walk, Walk::Refused, "an expansion record named where an index belongs was not refused");
    std::println!("reserved tags: an expansion named as an index is refused");
}

/// The control (rule 23): the same history with the look-alike under the value the expansion
/// records had before segment format 3 (the index's): the walk follows it, the targets no index
/// names outlive the sweep, and the oracle says so.
/// Prints "reserved tags control: the shared value is followed".
#[test]
fn the_oracle_catches_an_expansion_under_the_index_tag() {
    let root = test_root();
    let walk = history(SHARED_BEFORE, &root).expect("the history runs");
    assert_eq!(walk, Walk::Followed, "the shared value was not followed: the oracle cannot see the bug");
    std::println!("reserved tags control: the shared value is followed");
}

/// The invariant, both directions over the whole registry and a kind's tag: the walk follows a
/// record where an index belongs exactly when the registry says it is an index
/// ([`reach_of`]` == `[`TagReach::IndexKeys`]); every other record there is refused.
/// Prints "reserved tags: the walk follows exactly the index (<n> tags)".
#[test]
fn the_walk_follows_exactly_what_the_registry_calls_an_index() {
    let root = test_root();
    let mut tags: Vec<Tag> = ReservedTag::ALL.iter().map(|it| it.tag()).collect();
    tags.push(Tag(1));
    let mut wrong = Vec::new_in(Global);
    for &tag in &tags {
        let walk = history(tag, &root).unwrap_or_else(|why| panic!("the history of tag {}: {why}", tag.0));
        let want = if reach_of(tag) == TagReach::IndexKeys { Walk::Followed } else { Walk::Refused };
        if walk != want {
            wrong.push((tag.0, walk, want));
        }
    }
    assert!(wrong.is_empty(), "(tag, walked, registry): {wrong:?}");
    std::println!("reserved tags: the walk follows exactly the index ({} tags)", tags.len());
}

/// The round trip (rule 26): a record of every reserved tag, put and committed, reads back in a
/// fresh process with its own tag, namespace and bytes.
/// Prints "reserved tags: <n> tagged records round trip".
#[test]
fn every_reserved_tag_round_trips_through_the_store() {
    let root = test_root();
    let mut w = World { sim: Sim::new(71, Faults::default()), next: 0, plant: Plant::None, root: &root };
    let tagged: Vec<(u32, ReservedTag)> = ReservedTag::ALL.iter().enumerate().map(|(at, it)| (40_000 + at as u32, *it)).collect();
    w.command(
        &[],
        |store| {
            for &(n, tag) in &tagged {
                store.put(key(n), Namespace::LOCAL, tag.tag(), &value(n)).map_err(|why| format!("the put of {}: {why:?}", tag.name()))?;
            }
            Ok(())
        },
        false,
    )
    .expect("the command commits");
    let r = w.open(Plant::None).expect("a reader opens");
    for &(n, tag) in &tagged {
        let got = r.get(&key(n)).unwrap_or_else(|| panic!("the {} record does not read back", tag.name()));
        assert_eq!((got.tag, got.namespace, got.bytes), (tag.tag(), Namespace::LOCAL, &value(n)[..]), "the {} record", tag.name());
    }
    std::println!("reserved tags: {} tagged records round trip", tagged.len());
}
