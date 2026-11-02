use std::collections::BTreeMap;

use super::*;

pub(crate) fn id(n: u32) -> FnId {
    FnId(ContentId::of("blaze v2 testsel test fn 1", &n.to_le_bytes()))
}

pub(crate) fn bound(n: u32, version: u32) -> Bound {
    Bound(ContentId::of("blaze v2 testsel test bound 1", &[n.to_le_bytes(), version.to_le_bytes()].concat()))
}

fn version(n: u32, v: u32) -> Version {
    Version { id: id(n), bound: bound(n, v) }
}

#[test]
fn the_lookup_finds_what_a_map_finds() {
    for count in [0u32, 1, 2, 7, 8, 9, 100, 1000] {
        let fns: Vec<Version> = (0..count).map(|n| version(n, n % 3)).collect();
        let image = Image::new(fns.clone(), Vec::new(), ContentId::default()).unwrap();
        let map: BTreeMap<FnId, Bound> = fns.iter().map(|it| (it.id, it.bound)).collect();
        for n in 0..count + 50 {
            assert_eq!(image.bound_of(&id(n)), map.get(&id(n)).copied(), "{n} of {count}");
        }
    }
}

#[test]
fn a_table_that_is_not_one_is_refused() {
    let fns = vec![version(0, 0), version(1, 0)];
    assert_eq!(Image::new(fns.clone(), vec![2], ContentId::default()).err(), Some(TableError::NoFunction { slot: 0, function: 2 }));
    assert_eq!(Image::new(fns.clone(), vec![1, 1], ContentId::default()).err(), Some(TableError::TwoSlots { function: 1 }));
    assert_eq!(Image::new(vec![version(0, 0), version(0, 1)], vec![], ContentId::default()).err(), Some(TableError::Twice(id(0))));
    let image = Image::new(fns, vec![1], ContentId::default()).unwrap();
    assert_eq!(image.reached(&[5]).err(), Some(TableError::NotInTable(5)));
    assert_eq!(image.executed(2, &[0]).err(), Some(TableError::OtherBinary { record: 2, table: 1 }));
}

#[test]
fn an_emptied_slot_is_counted_and_a_hit_on_it_refused() {
    let fns = vec![version(0, 0), version(1, 0), version(2, 0)];
    let image = Image::new(fns.clone(), vec![0, NONE, 1], ContentId::default()).unwrap();
    assert_eq!(image.slots(), 3);
    assert_eq!(image.executed(3, &[1]).err(), Some(TableError::EmptySlot { slot: 1 }));
    assert_eq!(image.executed(3, &[2, 0]).unwrap().fns, image.executed(3, &[0, 2]).unwrap().fns);
    assert_eq!(image.executed(3, &[0, 2]).unwrap().fns.len(), 2);
    let whole = Image::new(fns.clone(), vec![0, 2, 1], ContentId::default()).unwrap();
    assert_ne!(image.unlogged(), whole.unlogged(), "function 2 is in no slot once its slot is emptied");
}

#[test]
fn the_unlogged_hash_is_of_what_has_no_slot_in_any_order() {
    let beside = ContentId::of("blaze v2 testsel test image 1", b"deps");
    let a = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0], beside).unwrap();
    let b = Image::new(vec![version(2, 0), version(0, 0), version(1, 0)], vec![1], beside).unwrap();
    assert_eq!(a.unlogged(), b.unlogged());
    // A logged function's edit leaves it; an unlogged one's, or the code beside, does not.
    let logged = Image::new(vec![version(0, 1), version(1, 0), version(2, 0)], vec![0], beside).unwrap();
    assert_eq!(a.unlogged(), logged.unlogged());
    let unlogged = Image::new(vec![version(0, 0), version(1, 1), version(2, 0)], vec![0], beside).unwrap();
    assert_ne!(a.unlogged(), unlogged.unlogged());
    let other = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0], ContentId::default()).unwrap();
    assert_ne!(a.unlogged(), other.unlogged());
    // Moving a function behind a slot changes what is unlogged.
    let moved = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0, 1], beside).unwrap();
    assert_ne!(a.unlogged(), moved.unlogged());
}

#[test]
fn sets_are_sorted_round_trip_and_name_what_changed() {
    let fns: Vec<Version> = (0..10).map(|n| version(n, 0)).collect();
    let slots: Vec<u32> = (0..8).rev().collect();
    let image = Image::new(fns, slots, ContentId::default()).unwrap();
    let set = image.executed(8, &[0, 3, 7]).unwrap();
    assert_eq!(set.fns.len(), 3);
    assert!(set.fns.windows(2).all(|it| it[0] < it[1]));
    let mut want: Vec<Version> = [7, 4, 0].iter().map(|n| version(*n, 0)).collect();
    want.sort();
    assert_eq!(set.fns, want);
    let bytes = set.encode();
    let view = SetView::decode(&bytes).unwrap();
    assert_eq!(view.to_set(), set);
    assert_eq!(view.len(), 3);
    assert!(SetView::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(SetView::decode(b"blaze v2 fnset 2").is_err());
    assert_eq!(image.changed(&view), None);
    // The reached set leaves out what has no slot (8 and 9).
    assert_eq!(image.reached(&[9, 8, 7, 4, 0, 4]).unwrap(), set);

    let edited: Vec<Version> = (0..10).map(|n| version(n, u32::from(n == 4))).collect();
    let now = Image::new(edited, (0..8).rev().collect(), ContentId::default()).unwrap();
    assert_eq!(now.changed(&view), Some(Change::Changed(id(4))));
    let fewer: Vec<Version> = (0..10).filter(|n| *n != 7).map(|n| version(n, 0)).collect();
    let gone = Image::new(fewer, (0..7).collect(), ContentId::default()).unwrap();
    assert_eq!(gone.changed(&view), Some(Change::Gone(id(7))));
    let unlogged: Vec<Version> = (0..10).map(|n| version(n, u32::from(n == 9))).collect();
    let deps = Image::new(unlogged, (0..8).rev().collect(), ContentId::default()).unwrap();
    assert_eq!(deps.changed(&view), Some(Change::Unlogged));
}

/// The unlogged hash as it was first written: 64-byte compares, a call per identity and bound.
fn unlogged_reference(fns: &[Version], slot_fn: &[u32], beside: ContentId) -> ContentId {
    let mut outside: Vec<&Version> =
        fns.iter().enumerate().filter(|(at, _)| !slot_fn.contains(&(*at as u32))).map(|it| it.1).collect();
    outside.sort_unstable();
    let mut hasher = Hasher::new(UNLOGGED_DOMAIN);
    hasher.id(&beside).update(&(outside.len() as u64).to_le_bytes());
    for version in outside {
        hasher.id(&version.id.0).id(&version.bound.0);
    }
    hasher.finish()
}

/// Identities whose first eight bytes are equal, so the order falls to the rest.
fn twin(n: u32, prefix: [u8; 8]) -> Version {
    let mut id = id(n).0;
    id.0[..8].copy_from_slice(&prefix);
    Version { id: FnId(id), bound: bound(n, 0) }
}

#[test]
fn the_unlogged_hash_and_order_are_the_reference_ones_at_every_size() {
    let mut rng = blaze_ids::rng::SplitMix64::new(3);
    for count in [0u32, 1, 2, 63, 64, 65, 255, 256, 257, 1000, 5000] {
        let mut fns: Vec<Version> = (0..count).map(|n| version(n, n % 5)).collect();
        if count > 10 {
            fns[3] = twin(3, [7; 8]);
            fns[9] = twin(9, [7; 8]);
            fns[10] = twin(10, [7; 8]);
        }
        let slot_fn: Vec<u32> = (0..count).filter(|_| rng.below(3) == 0).collect();
        let beside = ContentId::of("blaze v2 testsel test image 1", &count.to_le_bytes());
        let mut sorted = fns.clone();
        sorted.sort_unstable();
        // A table in codegen's order, and one in identity order already.
        for fns in [fns, sorted.clone()] {
            let image = Image::new(fns.clone(), slot_fn.clone(), beside).unwrap();
            assert_eq!(image.unlogged(), unlogged_reference(&fns, &slot_fn, beside), "{count}");
            let ordered: Vec<Version> = image.order.iter().map(|&at| fns[at as usize]).collect();
            assert_eq!(ordered, sorted, "{count}");
            for (at, &rank) in image.rank.iter().enumerate() {
                assert_eq!(image.order[rank as usize] as usize, at);
            }
        }
    }
}

/// Both ways a set is made (a bitset for many, a sort for few) against `Set::of`.
#[test]
fn sets_of_ranks_are_sets_of_versions() {
    let mut rng = blaze_ids::rng::SplitMix64::new(5);
    for count in [1u32, 2, 64, 200, 3000] {
        let fns: Vec<Version> = (0..count).map(|n| version(n, n % 2)).collect();
        let slots: Vec<u32> = (0..count).rev().collect();
        let image = Image::new(fns.clone(), slots.clone(), ContentId::default()).unwrap();
        for many in [0usize, 1, 3, 20, count as usize, 3 * count as usize] {
            let hits: Vec<u32> = (0..many).map(|_| rng.below(u64::from(count)) as u32).collect();
            let want = Set::of(image.unlogged(), hits.iter().map(|&at| fns[at as usize]).collect());
            assert_eq!(image.reached(&hits).unwrap(), want, "{count} {many}");
            let mut slot_hits: Vec<u32> = hits.iter().map(|&at| count - 1 - at).collect();
            slot_hits.sort_unstable();
            slot_hits.dedup();
            assert_eq!(image.executed(count, &slot_hits).unwrap(), want, "{count} {many}");
        }
    }
}

#[test]
fn a_record_keys_by_what_it_executed_only_when_whole() {
    let image = Image::new(vec![version(0, 0), version(1, 0)], vec![0, 1], ContentId::default()).unwrap();
    let whole = Record::Complete { slots: 2, hits: vec![1] };
    assert_eq!(image.of_record(&whole), Some(Ok(Set::of(image.unlogged(), vec![version(1, 0)]))));
    assert_eq!(image.of_record(&Record::incomplete("killed")), None);
}
