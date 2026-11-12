//! Item sets, the live form ([`crate::sets`]): the listing's layout, what one item more
//! writes, and the collector's scan following a kept root to its items.

use std::collections::BTreeSet;

use super::*;
use crate::collect::{Pin, SET_NOT_FOLLOWED};
use crate::sets::{self, Listing, Sets};

fn set_id(n: u64) -> ContentId {
    ContentId::of("blaze v2 store test set 1", &n.to_le_bytes())
}

/// An item's bytes: 16 to 200 of them, that do not compress.
fn item(n: u64) -> Vec<u8> {
    noise(16 + (n as usize * 7) % 185, n)
}

fn sorted(keys: impl Iterator<Item = u64>) -> Vec<Key> {
    let mut keys: Vec<Key> = keys.map(key).collect();
    keys.sort_unstable();
    keys
}

/// Rule persisted-state-round-trips, for the listing: a leaf and a root read back as they
/// were written, byte for byte when written again; a listing of the layout before (its
/// version byte), a cut one, one with bytes over and a leaf out of order are each refused
/// with why, never read as empty.
#[test]
fn a_listing_round_trips_and_another_layout_is_refused() {
    let keys = sorted(0..300);
    let (leaves, (root_key, root)) = sets::listing_of(&set_id(1), &keys, &[key(900), key(901)]);
    assert!(leaves.len() >= 2, "three hundred keys are more than one leaf");
    let mut listed = Vec::new();
    for (leaf_key, bytes) in &leaves {
        let Listing::Leaf(named) = sets::decode(bytes).unwrap() else { panic!("a leaf") };
        assert_eq!(sets::encode_leaf(&named), *bytes);
        assert_eq!(*leaf_key, Key(ContentId::of("blaze v2 store set leaf 1", bytes)));
        // Every leaf but the last ends at a key that ends leaves, and holds no other.
        let ends = named.iter().filter(|it| sets::ends_leaf(it)).count();
        assert!(ends <= 1 && named[..named.len() - 1].iter().all(|it| !sets::ends_leaf(it)));
        listed.extend(named);
    }
    assert_eq!(listed, keys, "the leaves hold every key once, in order");
    let Listing::Root { set, leaves: named, sealed } = sets::decode(&root).unwrap() else {
        panic!("a root")
    };
    assert_eq!((set, sealed), (set_id(1), vec![key(900), key(901)]));
    assert_eq!(named, leaves.iter().map(|it| it.0).collect::<Vec<_>>());
    assert_eq!(sets::encode_root(&set, &named, &[key(900), key(901)]), root);
    assert_eq!(root_key, Key(ContentId::of("blaze v2 store set root 1", &root)));
    // The same keys under another set: the same leaves, another root.
    let (again, (other_root, _)) = sets::listing_of(&set_id(2), &keys, &[key(900), key(901)]);
    assert_eq!(again, leaves);
    assert_ne!(other_root, root_key);

    let mut before = root.clone();
    before[7] = 0;
    let why = sets::decode(&before).unwrap_err();
    assert!(why.contains("a listing of layout 0, and this blaze reads layout 1"), "{why}");
    assert!(sets::decode(&root[..root.len() - 1]).is_err(), "a cut root");
    assert!(sets::decode(&[root.as_slice(), &[0]].concat()).is_err(), "a root with a byte over");
    assert!(sets::decode(&root[..10]).is_err(), "under a header");
    assert!(sets::decode(b"BLZPIN\x00\x01 and more bytes").unwrap_err().contains("not a listing"));
    let mut swapped = vec![keys[1], keys[0]];
    swapped.extend(&keys[2..5]);
    let mut leaf = sets::encode_leaf(&keys[..5]);
    leaf[16..80].copy_from_slice(&[swapped[0].0.0, swapped[1].0.0].concat());
    assert!(sets::decode(&leaf).unwrap_err().contains("do not ascend"));
}

/// The count test: 50,000 items put as a live set and listed, then one more. The second
/// listing puts a leaf or two (two when the new key ends a leaf) and the root, the store
/// grows by the item, those listings and no more, and every other leaf is the record it was.
/// The control is the form before: a pin naming the items directly is written again whole,
/// 32 bytes an item, many times what the listing wrote.
#[test]
fn one_item_more_of_fifty_thousand_writes_one_record_a_leaf_and_the_root() {
    const ITEMS: u64 = 50_000;
    let mut sim = Sim::new(11, quiet());
    let mut store = open(&mut sim, "store");
    let handle = Handle::new(&mut store, "/sim".into());
    let sets = Sets::new();
    let set = set_id(7);
    for n in 0..ITEMS {
        sets.put(&handle, &set, key(n).0, &item(n)).unwrap();
    }
    let (roots, first) = sets.list(&handle).unwrap();
    assert_eq!((first.sets, first.changed, roots.len()), (1, 1, 1));
    assert_eq!(sets.root(&set), Some(roots[0].1));
    flush(&mut sim, &mut store);
    let before = store.usage().bytes;
    let (old_leaves, _) = sets::listing_of(&set, &sorted(0..ITEMS), &[]);
    assert_eq!(first.listings, old_leaves.len() as u64 + 1);

    // Nothing touched: nothing listed. A get alone: listed, nothing changed, nothing put.
    assert_eq!(sets.list(&handle).unwrap().1, sets::Listed::default());
    let mut read = Vec::new();
    assert!(sets.get(&handle, &set, &key(5).0, &mut |bytes| read = bytes.to_vec()).unwrap());
    assert_eq!(read, item(5));
    assert!(!sets.get(&handle, &set, &key(ITEMS + 9).0, &mut |_| panic!("nothing is kept")).unwrap());
    let (same, got) = sets.list(&handle).unwrap();
    assert_eq!((got.sets, got.changed, got.listings), (1, 0, 0));
    assert_eq!(same, roots);

    sets.put(&handle, &set, key(ITEMS).0, &item(ITEMS)).unwrap();
    let (roots, second) = sets.list(&handle).unwrap();
    flush(&mut sim, &mut store);
    let (new_leaves, (new_root, root_bytes)) = sets::listing_of(&set, &sorted(0..=ITEMS), &[]);
    assert_eq!(roots, vec![(set, new_root)]);
    let old: BTreeSet<Key> = old_leaves.iter().map(|it| it.0).collect();
    let fresh: Vec<&(Key, Vec<u8>)> = new_leaves.iter().filter(|it| !old.contains(&it.0)).collect();
    assert!((1..=2).contains(&fresh.len()), "{} leaves are new", fresh.len());
    // What the store grew by: the item, the new leaves and the root, each with a record's
    // header and padding, and each pack's commit padded to a block.
    let values = item(ITEMS).len() as u64
        + fresh.iter().map(|it| it.1.len() as u64).sum::<u64>()
        + root_bytes.len() as u64;
    let records = fresh.len() as u64 + 2;
    let grew = store.usage().bytes - before;
    assert!(grew >= values, "the item and its listings were written: {grew} of {values}");
    assert!(
        grew <= values + records * (crate::RECORD_HEADER as u64 + 16) + 3 * crate::disk::BLOCK,
        "the store grew by {grew} bytes for {values} of values in {records} records"
    );
    // The control: the same change to a pin that names the items.
    let pin = Pin { name: "set".into(), why: "the control".into(), when: 1, keys: sorted(0..=ITEMS) };
    let pinned = pin.encode().len() as u64;
    assert!(pinned >= 32 * (ITEMS + 1));
    assert!(
        second.bytes * 20 < pinned,
        "the listing put {} bytes where a pin of the items is {pinned}",
        second.bytes
    );
    // Every item still reads by its key, under this set or any other.
    for n in [0, 1, ITEMS / 2, ITEMS] {
        let mut read = Vec::new();
        assert!(sets.get(&handle, &set_id(8), &key(n).0, &mut |bytes| read = bytes.to_vec()).unwrap());
        assert_eq!(read, item(n));
    }
    // And is a member of the set it was got under, which is listed with the four of them.
    let (roots, third) = sets.list(&handle).unwrap();
    assert_eq!((third.sets, third.changed, third.listings), (1, 1, 2));
    assert_eq!(roots, vec![(set_id(8), sets::listing_of(&set_id(8), &sorted([0, 1, ITEMS / 2, ITEMS].into_iter()), &[]).1.0)]);
    drop(handle);
}

/// What a scan of a store with a set measured as kept by a root, after the periods that
/// clear every flag of use: (the bytes of every record in a pack, those a root keeps, the
/// bytes of the set's items as they were put, those of the items in no set).
fn kept_by_root(pinned: bool, followed: bool) -> (u64, u64, u64, u64) {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    let (mut in_set, mut outside) = (0, 0);
    for n in 0..300 {
        store.put(key(n), Kind::Query, &item(n)).unwrap();
        in_set += item(n).len() as u64;
    }
    for n in 1000..1050 {
        store.put(key(n), Kind::Query, &item(n)).unwrap();
        outside += item(n).len() as u64;
    }
    let (leaves, root) = sets::listing_of(&set_id(3), &sorted(0..300), &[]);
    for (key, bytes) in leaves.iter().chain([&root]) {
        store.put(*key, Kind::Set, bytes).unwrap();
    }
    // More than an index table's worth: every record is in the table, where a listing is
    // followed only for a root's sake (one still in the delta is followed whatever names it).
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].head.index.is_some(), "the records are in an index table");
    if pinned {
        store.pin("crate", "the test's", 1, &[root.0]).unwrap();
        flush(&mut sim, &mut store);
    }
    SET_NOT_FOLLOWED.with(|it| it.set(!followed));
    for period in 0..4 {
        if let Some(mut carry) = store.carry_take() {
            carry.run();
            store.carry_put(carry);
        }
        let mut scan = (0..8)
            .find_map(|_| store.plan_take())
            .unwrap_or_else(|| panic!("period {period}: the store can plan"));
        scan.run();
        assert!(store.plan_put(scan), "period {period}: the plan is taken");
        flush(&mut sim, &mut store);
    }
    SET_NOT_FOLLOWED.with(|it| it.set(false));
    let scanned = store.usage().scanned;
    assert!(store.problems().is_empty(), "{:?}", store.problems());
    (scanned.live_bytes, scanned.rooted_bytes, in_set, outside)
}

/// The invariant: while a set's root is kept, the collector's scan keeps every item its
/// listing names, and nothing for it that no listing names. A pin names the root alone.
/// Controls: with no pin nothing of the set is kept once its flags of use are gone; and a
/// collector that follows no listing (`SET_NOT_FOLLOWED`, the planted bug) keeps the pin's
/// root and none of the items, which a collection would then forget.
#[test]
fn a_kept_root_keeps_exactly_what_its_listing_names() {
    let (live, rooted, in_set, outside) = kept_by_root(true, true);
    assert!(rooted >= in_set, "every item of the set is kept: {rooted} of {in_set}");
    assert!(live - rooted >= outside, "no item outside the set is: {rooted} of {live}");
    let (_, rooted, in_set, _) = kept_by_root(false, true);
    assert!(rooted < in_set / 4, "with no pin the set's items are not kept: {rooted}");
    let (_, rooted, in_set, _) = kept_by_root(true, false);
    assert!(rooted < in_set / 4, "the planted bug keeps the root and no item: {rooted}");
}
