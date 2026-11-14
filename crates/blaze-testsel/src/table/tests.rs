use std::collections::BTreeMap;

use super::*;

pub(crate) fn id(n: u32) -> FnId {
    FnId(ContentId::of("blaze v2 testsel test fn 1", &n.to_le_bytes()))
}

pub(crate) fn bound(n: u32, version: u32) -> Bound {
    Bound(ContentId::of(
        "blaze v2 testsel test bound 1",
        &[n.to_le_bytes(), version.to_le_bytes()].concat(),
    ))
}

fn version(n: u32, v: u32) -> Version {
    Version { id: id(n), bound: bound(n, v) }
}

/// A jump table of `slots` slots, of one lineage and generation.
pub(crate) fn jump(slots: u32) -> Table {
    Table { lineage: [4; 16], generation: 2, slots }
}

#[test]
fn the_lookup_finds_what_a_map_finds() {
    for count in [0u32, 1, 2, 7, 8, 9, 100, 1000] {
        let fns: Vec<Version> = (0..count).map(|n| version(n, n % 3)).collect();
        let image = Image::new(fns.clone(), Vec::new(), ContentId::default(), jump(0)).unwrap();
        let map: BTreeMap<FnId, Bound> = fns.iter().map(|it| (it.id, it.bound)).collect();
        for n in 0..count + 50 {
            assert_eq!(image.bound_of(&id(n)), map.get(&id(n)).copied(), "{n} of {count}");
        }
    }
}

#[test]
fn a_table_that_is_not_one_is_refused() {
    let fns = vec![version(0, 0), version(1, 0)];
    let zero = ContentId::default();
    assert_eq!(
        Image::new(fns.clone(), vec![2], zero, jump(1)).err(),
        Some(TableError::NoFunction { slot: 0, function: 2 })
    );
    assert_eq!(
        Image::new(fns.clone(), vec![1, 1], zero, jump(2)).err(),
        Some(TableError::TwoSlots { function: 1 })
    );
    assert_eq!(
        Image::new(vec![version(0, 0), version(0, 1)], vec![], zero, jump(0)).err(),
        Some(TableError::Twice(id(0)))
    );
    assert_eq!(
        Image::new(fns.clone(), vec![0, 1], zero, jump(1)).err(),
        Some(TableError::PastTable { given: 2, slots: 1 })
    );
    let image = Image::new(fns, vec![1], zero, jump(1)).unwrap();
    assert_eq!(image.reached(&[5]).err(), Some(TableError::NotInTable(5)));
    assert_eq!(
        image.executed(jump(2), &[0]).err(),
        Some(TableError::OtherBinary { record: jump(2), table: jump(1) })
    );
}

/// The invariant: a record is read against a binary's table only when the jump table it
/// names (what the binary's runtime said) is the one the table's link made, by identity. A
/// record of that table reads, a slot of it the link has not given out included as no
/// function; the control: the same hits in a record of the image's next generation, of
/// another image, or of a table of as many slots as the link gave out (the count the check
/// once compared with the runtime's capacity: 21059 against 86595 on a real link) are
/// refused before a slot is read.
#[test]
fn a_record_is_read_only_against_the_table_it_names() {
    use crate::record::{HEADER, header, mark_armed};
    // A link that gave out 3 slots of a jump table of 70, its capacity.
    let linked = Table { lineage: [4; 16], generation: 2, slots: 70 };
    let fns = vec![version(0, 0), version(1, 0), version(2, 0)];
    let image = Image::new(fns, vec![2, 0, 1], ContentId::default(), linked).unwrap();
    assert_eq!((image.slots(), image.table()), (3, linked));
    // A region as the server maps it and the runtime arms it: a byte for each of the
    // table's slots, slot 1 called (and, in the second, slot 5, which the link never gave).
    let region = |of: &Table, set: &[u32]| {
        let mut bytes = header(of.slots).to_vec();
        bytes.resize(HEADER + of.slots as usize, 0);
        for &slot in set {
            bytes[HEADER + slot as usize] = 1;
        }
        assert!(mark_armed(&mut bytes));
        Record::of_region(&bytes, *of, Ok(()))
    };
    let record = region(&linked, &[1]);
    assert_eq!(record, Record::Complete { table: linked, hits: vec![1] });
    assert_eq!(image.of_record(&record), Some(Ok(Set::of(image.unlogged(), vec![version(0, 0)]))));
    assert_eq!(
        image.of_record(&region(&linked, &[1, 5])),
        Some(Err(TableError::EmptySlot { slot: 5 }))
    );
    for other in [
        Table { generation: 3, ..linked },
        Table { lineage: [5; 16], ..linked },
        Table { slots: 3, ..linked },
    ] {
        assert_eq!(
            image.of_record(&region(&other, &[1])),
            Some(Err(TableError::OtherBinary { record: other, table: linked })),
            "{other:?}"
        );
    }
    let said = TableError::OtherBinary { record: Table { generation: 3, ..linked }, table: linked };
    let said = said.to_string();
    assert!(said.contains("generation 3 of 70 slots") && said.contains("generation 2 of 70"), "{said}");
}

#[test]
fn an_emptied_slot_is_counted_and_a_hit_on_it_refused() {
    let fns = vec![version(0, 0), version(1, 0), version(2, 0)];
    let image = Image::new(fns.clone(), vec![0, NONE, 1], ContentId::default(), jump(3)).unwrap();
    assert_eq!(image.slots(), 3);
    assert_eq!(image.executed(jump(3), &[1]).err(), Some(TableError::EmptySlot { slot: 1 }));
    assert_eq!(
        image.executed(jump(3), &[2, 0]).unwrap().fns,
        image.executed(jump(3), &[0, 2]).unwrap().fns
    );
    assert_eq!(image.executed(jump(3), &[0, 2]).unwrap().fns.len(), 2);
    let whole = Image::new(fns.clone(), vec![0, 2, 1], ContentId::default(), jump(3)).unwrap();
    assert_ne!(
        image.unlogged(),
        whole.unlogged(),
        "function 2 is in no slot once its slot is emptied"
    );
}

#[test]
fn the_unlogged_hash_is_of_what_has_no_slot_in_any_order() {
    let beside = ContentId::of("blaze v2 testsel test image 1", b"deps");
    let t = jump(2);
    let a =
        Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0], beside, t).unwrap();
    let b =
        Image::new(vec![version(2, 0), version(0, 0), version(1, 0)], vec![1], beside, t).unwrap();
    assert_eq!(a.unlogged(), b.unlogged());
    // A logged function's edit leaves it; an unlogged one's, or the code beside, does not.
    let logged =
        Image::new(vec![version(0, 1), version(1, 0), version(2, 0)], vec![0], beside, t).unwrap();
    assert_eq!(a.unlogged(), logged.unlogged());
    let unlogged =
        Image::new(vec![version(0, 0), version(1, 1), version(2, 0)], vec![0], beside, t).unwrap();
    assert_ne!(a.unlogged(), unlogged.unlogged());
    let other = Image::new(
        vec![version(0, 0), version(1, 0), version(2, 0)],
        vec![0],
        ContentId::default(),
        t,
    )
    .unwrap();
    assert_ne!(a.unlogged(), other.unlogged());
    // Moving a function behind a slot changes what is unlogged.
    let moved =
        Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0, 1], beside, t)
            .unwrap();
    assert_ne!(a.unlogged(), moved.unlogged());
}

#[test]
fn sets_are_sorted_and_the_reached_set_leaves_out_what_has_no_slot() {
    let fns: Vec<Version> = (0..10).map(|n| version(n, 0)).collect();
    let slots: Vec<u32> = (0..8).rev().collect();
    let image = Image::new(fns, slots, ContentId::default(), jump(8)).unwrap();
    let set = image.executed(jump(8), &[0, 3, 7]).unwrap();
    assert_eq!(set.fns.len(), 3);
    assert!(set.fns.windows(2).all(|it| it[0] < it[1]));
    let mut want: Vec<Version> = [7, 4, 0].iter().map(|n| version(*n, 0)).collect();
    want.sort();
    assert_eq!(set.fns, want);
    // The reached set leaves out what has no slot (8 and 9).
    assert_eq!(image.reached(&[9, 8, 7, 4, 0, 4]).unwrap(), set);
}

/// The invariant: `changed_since` names exactly the functions whose bound code differs between
/// two binaries (gone and new ones included), and `UNLOGGED` exactly when the unlogged hash
/// moved; the program state is equal exactly when nothing is named.
#[test]
fn changes_since_a_state_are_the_functions_whose_bound_moved() {
    let image = |versions: &[Option<u32>]| {
        let fns: Vec<Version> =
            versions.iter().enumerate().filter_map(|(n, v)| v.map(|v| version(n as u32, v))).collect();
        let slots = (fns.len() as u32).min(4);
        Image::new(fns, (0..slots).collect(), ContentId::default(), jump(4)).unwrap()
    };
    let changes = |now: &Image, then: &Image| {
        let mut out = Vec::new_in(std::alloc::Global);
        now.changed_since(then, &mut out);
        let mut out = out.to_vec();
        out.sort();
        out
    };
    let before = image(&[Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)]);
    assert_eq!(changes(&before, &before), Vec::new());
    let same = image(&[Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)]);
    assert_eq!(same.state(), before.state());

    let edited = image(&[Some(0), Some(1), Some(0), Some(0), Some(0), Some(0)]);
    assert_eq!(changes(&edited, &before), vec![id(1)]);
    assert_ne!(edited.state(), before.state());
    // An undone edit is the old state.
    assert_eq!(changes(&before, &edited), vec![id(1)]);

    // Function 1 gone and 6 new: the first four functions have slots, so 4 moves from the
    // unlogged code to a slot, and the unlogged hash moves too.
    let gone_and_new = image(&[Some(0), None, Some(0), Some(0), Some(0), Some(0), Some(0)]);
    let mut want = vec![id(1), id(6), crate::ask::UNLOGGED];
    want.sort();
    assert_eq!(changes(&gone_and_new, &before), want);

    // Function 5 has no slot: its edit moves the unlogged hash too.
    let unlogged = image(&[Some(0), Some(0), Some(0), Some(0), Some(0), Some(1)]);
    let mut want = vec![id(5), crate::ask::UNLOGGED];
    want.sort();
    assert_eq!(changes(&unlogged, &before), want);
}

/// The unlogged hash as it was first written: 64-byte compares, a call per identity and bound.
fn unlogged_reference(fns: &[Version], slot_fn: &[u32], beside: ContentId) -> ContentId {
    let mut outside: Vec<&Version> = fns
        .iter()
        .enumerate()
        .filter(|(at, _)| !slot_fn.contains(&(*at as u32)))
        .map(|it| it.1)
        .collect();
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
            let image = Image::new(fns.clone(), slot_fn.clone(), beside, jump(count)).unwrap();
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
        let image = Image::new(fns.clone(), slots.clone(), ContentId::default(), jump(count)).unwrap();
        for many in [0usize, 1, 3, 20, count as usize, 3 * count as usize] {
            let hits: Vec<u32> = (0..many).map(|_| rng.below(u64::from(count)) as u32).collect();
            let want = Set::of(image.unlogged(), hits.iter().map(|&at| fns[at as usize]).collect());
            assert_eq!(image.reached(&hits).unwrap(), want, "{count} {many}");
            let mut slot_hits: Vec<u32> = hits.iter().map(|&at| count - 1 - at).collect();
            slot_hits.sort_unstable();
            slot_hits.dedup();
            assert_eq!(image.executed(jump(count), &slot_hits).unwrap(), want, "{count} {many}");
        }
    }
}

#[test]
fn a_record_keys_by_what_it_executed_only_when_whole() {
    let image =
        Image::new(vec![version(0, 0), version(1, 0)], vec![0, 1], ContentId::default(), jump(2))
            .unwrap();
    let whole = Record::Complete { table: jump(2), hits: vec![1] };
    assert_eq!(image.of_record(&whole), Some(Ok(Set::of(image.unlogged(), vec![version(1, 0)]))));
    assert_eq!(image.of_record(&Record::incomplete("killed")), None);
}
