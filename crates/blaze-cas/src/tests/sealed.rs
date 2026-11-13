//! Item sets, the sealed form ([`crate::sealed`], [`crate::sets::Sets::seal`]): the layout,
//! what a lookup reads, what an item costs, and a seal under crashes.

use super::*;
use crate::sealed::{self, Header, Probe};
use crate::sets::{self, Listing, SEALED_NOT_NAMED, Sets};

fn set_id(n: u64) -> ContentId {
    ContentId::of("blaze v2 store test sealed set 1", &n.to_le_bytes())
}

/// An item's bytes: 16 to 200 of them, that do not compress.
fn item(n: u64) -> Vec<u8> {
    noise(16 + (n as usize * 7) % 185, n)
}

/// The items `0..count`, ascending by key.
fn items(count: u64) -> Vec<(Key, Vec<u8>)> {
    let mut items: Vec<(Key, Vec<u8>)> = (0..count).map(|n| (key(n), item(n))).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

fn encoded(set: u64, items: &[(Key, Vec<u8>)]) -> Vec<u8> {
    let refs: Vec<(Key, &[u8])> = items.iter().map(|(key, bytes)| (*key, bytes.as_slice())).collect();
    sealed::encode(&set_id(set), &refs).unwrap()
}

/// Rule persisted-state-round-trips, for the sealed set: its items read back as they were
/// written and write the same bytes again; one of the layout before (its number), a cut
/// one, one with bytes over, one whose slot names no item and one with a byte of padding
/// set are each refused with why, never read as empty. The empty set round-trips too.
#[test]
fn a_sealed_set_round_trips_and_another_layout_is_refused() {
    for count in [0, 1, 7, 300] {
        let items = items(count);
        let bytes = encoded(1, &items);
        let (header, read) = sealed::decode(&bytes).unwrap();
        assert_eq!((header.count as u64, header.set), (count, set_id(1)));
        assert_eq!(header, Header::decode(&bytes[..sealed::FIXED]).unwrap());
        let read: Vec<(Key, Vec<u8>)> = read.into_iter().map(|(key, bytes)| (key, bytes.to_vec())).collect();
        assert_eq!(read, items);
        assert_eq!(encoded(1, &read), bytes, "{count} items written again are the same bytes");
        assert_ne!(sealed::key_of(&encoded(2, &items)), sealed::key_of(&bytes), "another set, another key");
    }
    let bytes = encoded(1, &items(300));
    let mut before = bytes.clone();
    before[7] = 0;
    let why = sealed::decode(&before).unwrap_err();
    assert!(why.contains("a sealed set of layout 0, and this blaze reads layout 1"), "{why}");
    assert!(Header::decode(&before[..sealed::FIXED]).unwrap_err().contains("layout 0"));
    assert!(sealed::decode(&bytes[..bytes.len() - 8]).is_err(), "a cut one");
    assert!(sealed::decode(&[bytes.as_slice(), &[0; 8]].concat()).is_err(), "one with bytes over");
    assert!(sealed::decode(&bytes[..20]).is_err(), "under a header");
    assert!(sealed::decode(b"BLZSET\x00\x01 and enough more bytes to be as long as a header is")
        .unwrap_err()
        .contains("not a sealed set"));
    // A slot that names an offset inside the directory.
    let header = Header::decode(&bytes).unwrap();
    let full = (0..header.slots as usize)
        .map(|n| sealed::FIXED + n * sealed::SLOT)
        .find(|at| bytes[at + 8..at + 16] != [0; 8])
        .unwrap();
    let mut wrong = bytes.clone();
    wrong[full + 8..full + 16].copy_from_slice(&56u64.to_le_bytes());
    assert!(sealed::decode(&wrong).is_err(), "a slot that names no item");
    // An item of 16 bytes ends on a multiple of 8; one of 23 has a byte of padding.
    let odd = encoded(1, &[(key(1), vec![7; 23])]);
    let mut set = odd.clone();
    *set.last_mut().unwrap() = 1;
    assert!(sealed::decode(&set).unwrap_err().contains("padding"));
}

/// What lookups of every item of a sealed set of `count`, and of as many keys that are not
/// its items, read: (the most parts one that found its item read, the mean parts of those
/// in hundredths, the most bytes one read over its item's own, the mean parts of the misses
/// in hundredths).
fn lookups(count: u64) -> (u32, u64, u64, u64) {
    let items = items(count);
    let bytes = encoded(3, &items);
    let header = Header::decode(&bytes).unwrap();
    assert_eq!(header.slots, sealed::slots_for(count as u32), "keys that spread need no more slots");
    let (mut most, mut parts, mut over, mut missed) = (0, 0u64, 0, 0u64);
    for (key, value) in &items {
        let mut probe = Probe::default();
        let found = sealed::find(bytes.as_slice(), &header, key, &mut probe).unwrap();
        assert_eq!(found, Some(value.as_slice()));
        most = most.max(probe.parts);
        parts += probe.parts as u64;
        over = over.max(probe.bytes - value.len() as u64);
    }
    for n in count..2 * count {
        let mut probe = Probe::default();
        assert_eq!(sealed::find(bytes.as_slice(), &header, &key(n), &mut probe).unwrap(), None);
        assert!(probe.parts <= sealed::MAX_RUNS + 2, "{probe:?}");
        missed += probe.parts as u64;
    }
    (most, parts * 100 / count, over, missed * 100 / count)
}

/// The invariant: a lookup in a sealed set reads a bounded number of parts whatever the
/// set's size: a run of slots or two and the item, each part a page or two of the pool.
/// Measured at 1,000 items and at 50,000: the same bounds hold at both, so what a get costs
/// does not grow with the set. (The pool's own count of pages loaded is the runner's
/// measurement; this counts what is asked of it.)
#[test]
fn a_lookup_in_a_sealed_set_reads_a_bounded_number_of_parts_at_two_sizes() {
    for count in [1_000, 50_000] {
        let (most, mean, over, missed) = lookups(count);
        assert!(mean <= 250, "{count} items: {mean} hundredths of a part a lookup that finds");
        assert!(most <= 24, "{count} items: {most} parts for one lookup");
        // Its runs of slots and its key, and no other item's bytes.
        assert!(over <= most as u64 * (sealed::RUN as u64 * sealed::SLOT as u64) + 32, "{count} items: {over}");
        assert!(missed <= 400, "{count} items: {missed} hundredths of a part a lookup that misses");
    }
}

/// The count test of the sealed form: 50,000 items put under a set written sealed at once
/// are one store value and a root. The store grows by the items' bytes and under 64 bytes
/// an item more (a slot at a load of 3/4, the key, padding), where a record each is over
/// 170; every item reads back under its set, held and then sealed; after the store is
/// opened again a daemon that adopts the root reads them; a second set that follows the
/// first reads them and names the sealed set in its own root. The control: the same items
/// under a live set are found under any set, these are not (the sharing given up).
#[test]
fn fifty_thousand_items_sealed_at_once_are_one_value() {
    const ITEMS: u64 = 50_000;
    let mut sim = Sim::new(13, quiet());
    let mut store = open(&mut sim, "store");
    let handle = Handle::new(&mut store, "/sim".into());
    let sets = Sets::new();
    let set = set_id(7);
    sets.seal_at_once(&set);
    flush(&mut sim, &mut store);
    let before = store.usage().bytes;
    let mut values = 0;
    for n in 0..ITEMS {
        sets.put(&handle, &set, key(n).0, &item(n)).unwrap();
        values += item(n).len() as u64;
    }
    let read = |sets: &Sets, set: &ContentId, n: u64| {
        let mut read = None;
        let found = sets.get(&handle, set, &key(n).0, &mut |bytes| read = Some(bytes.to_vec())).unwrap();
        assert_eq!(found, read.is_some());
        read
    };
    assert_eq!(read(&sets, &set, 5), Some(item(5)), "held, it reads under its set");
    assert_eq!(read(&sets, &set_id(8), 5), None, "and under no other");
    assert!(sets.put(&handle, &set, key(5).0, b"other bytes").unwrap_err().contains("was refused"));
    let sealing = sets.seal(&handle, &set).unwrap().expect("the set holds items");
    assert_eq!((sealing.items, sealing.merged, sealing.gone), (ITEMS, 0, 0));
    assert_eq!(sets.root(&set), Some(sealing.root));
    assert_eq!(sets.seal(&handle, &set).unwrap(), None, "nothing more to seal");
    flush(&mut sim, &mut store);
    assert!(handle.durable(sealing.durable));
    let grew = store.usage().bytes - before;
    assert!(grew >= values + 32 * ITEMS, "{grew} bytes for {values} of values");
    assert!(
        grew <= values + 64 * ITEMS + 4 * crate::disk::BLOCK,
        "the store grew by {grew} bytes for {values} of values: {} an item over its bytes",
        (grew - values) / ITEMS
    );
    for n in (0..ITEMS).step_by(97) {
        assert_eq!(read(&sets, &set, n), Some(item(n)), "item {n}, sealed");
    }
    assert_eq!(read(&sets, &set, ITEMS + 1), None);
    // The next version of the crate: another set that follows this one.
    let next = set_id(9);
    sets.follows(&next, &set);
    assert_eq!(read(&sets, &next, 11), Some(item(11)));
    sets.put(&handle, &next, key(ITEMS).0, &item(ITEMS)).unwrap();
    let (roots, listed) = sets.list(&handle).unwrap();
    assert_eq!((listed.changed, listed.listings), (1, 2), "its one new item's leaf and its root");
    let next_root = roots.iter().find(|it| it.0 == next).unwrap().1;
    let named = handle.read(&next_root, |entry| sets::decode(entry.bytes).unwrap()).unwrap().unwrap();
    let Listing::Root { sealed: named, leaves, .. } = named else { panic!("a root") };
    assert_eq!((named, leaves.len()), (vec![sealing.sealed], 1));
    flush(&mut sim, &mut store);
    drop(handle);
    drop(store);

    // A daemon started again knows the set by the root a pin or a build's record names.
    let mut store = open(&mut sim, "store");
    let handle = Handle::new(&mut store, "/sim".into());
    let sets = Sets::new();
    let read = |set: &ContentId, n: u64| {
        let mut read = None;
        sets.get(&handle, set, &key(n).0, &mut |bytes| read = Some(bytes.to_vec())).unwrap();
        read
    };
    assert_eq!(read(&set, 5), None, "not adopted yet: the set is not known");
    assert_eq!(sets.adopt(&handle, sealing.root).unwrap(), Some(set));
    assert_eq!(sets.adopt(&handle, key(5)).unwrap(), None, "a key that is no root");
    assert_eq!(sets.adopt(&handle, next_root).unwrap(), Some(next));
    for n in (0..ITEMS).step_by(997) {
        assert_eq!(read(&set, n), Some(item(n)), "item {n}, after a reopen");
        assert_eq!(read(&next, n), Some(item(n)), "item {n}, under the set that follows");
    }
    assert_eq!(read(&next, ITEMS), Some(item(ITEMS)));
    // The adopted set, touched: its next listing names what its root named, and one more.
    sets.put(&handle, &next, key(ITEMS + 1).0, &item(ITEMS + 1)).unwrap();
    let (roots, _) = sets.list(&handle).unwrap();
    let again = roots.iter().find(|it| it.0 == next).unwrap().1;
    let mut both = vec![key(ITEMS), key(ITEMS + 1)];
    both.sort_unstable();
    assert_eq!(again, sets::listing_of(&next, &both, &[sealing.sealed]).1.0);
    drop(handle);
}

const LIVE: u64 = 300;

/// Every item `root` keeps, by what it names: the records its leaves list and the items of
/// its sealed sets. An item listed whose record is not there is an error.
fn kept_by(handle: &Handle, root: Key) -> Result<BTreeMap<Key, Vec<u8>>, String> {
    let listing = |key: Key| {
        handle
            .read(&key, |entry| sets::decode(entry.bytes))
            .map_err(|corrupt| corrupt.to_string())?
            .ok_or_else(|| format!("the listing {:?} is named and not in the store", key.0))?
    };
    let Listing::Root { leaves, sealed: sealed_sets, .. } = listing(root)? else {
        return Err("the pin names a leaf".into());
    };
    let mut kept = BTreeMap::new();
    for leaf in leaves {
        let Listing::Leaf(keys) = listing(leaf)? else { return Err("a root names a root".into()) };
        for key in keys {
            let bytes = handle
                .read(&key, |entry| entry.bytes.to_vec())
                .map_err(|corrupt| corrupt.to_string())?
                .ok_or_else(|| format!("the item {:?} is listed and not in the store", key.0))?;
            kept.insert(key, bytes);
        }
    }
    for sealed_key in sealed_sets {
        handle
            .read(&sealed_key, |entry| {
                sealed::decode(entry.bytes).map(|(_, items)| {
                    kept.extend(items.into_iter().map(|(key, bytes)| (key, bytes.to_vec())));
                })
            })
            .map_err(|corrupt| corrupt.to_string())?
            .ok_or_else(|| format!("the sealed set {:?} is named and not in the store", sealed_key.0))??;
    }
    Ok(kept)
}

/// A flush whose commit stops being completed after `cut` completions: whether it was cut
/// (false: the commit ended in fewer).
fn flush_cut(sim: &mut Sim, store: &mut Store, cut: u64) -> bool {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    let mut done = 0;
    run_until_idle(sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) && done < cut {
            done += 1;
            _ = store.complete(completion, effects);
        }
    })
    .expect("the effects end");
    done >= cut
}

/// A live set of [`LIVE`] items whose root a pin names is compacted: sealed, the seal made
/// durable, then the pin moved to the new root and that made durable. With `cut`
/// `(stage, n)` the power goes after `n` completions of the commit of stage 0 (the seal's)
/// or 1 (the pin's). Then the store is opened again and judged by the oracle: the root the
/// name's newest pin names keeps every item of the set, in a record or in a sealed set, with
/// its bytes, and a daemon that adopts that root reads each. `Ok(whether the cut came)`.
fn compacted(seed: u64, cut: Option<(u8, u64)>, plant: bool) -> Result<bool, String> {
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    let handle = Handle::new(&mut store, "/sim".into());
    let sets = Sets::new();
    let set = set_id(seed);
    for n in 0..LIVE {
        sets.put(&handle, &set, key(n).0, &item(n))?;
    }
    let (roots, _) = sets.list(&handle)?;
    store.pin("crate", "the test's", 1, &[roots[0].1]).unwrap();
    flush(&mut sim, &mut store);

    SEALED_NOT_NAMED.with(|it| it.set(plant));
    let sealing = sets.seal(&handle, &set);
    SEALED_NOT_NAMED.with(|it| it.set(false));
    let sealing = sealing?.ok_or("the set held nothing to seal")?;
    let cut_at = |stage: u8| cut.filter(|it| it.0 == stage).map_or(u64::MAX, |it| it.1);
    let mut was_cut = flush_cut(&mut sim, &mut store, cut_at(0));
    if !was_cut {
        if !handle.durable(sealing.durable) {
            return Err("the seal's commit ended and its ticket is not durable".into());
        }
        store.pin("crate", "the test's", 2, &[sealing.root]).unwrap();
        was_cut = flush_cut(&mut sim, &mut store, cut_at(1));
    }
    drop(handle);
    drop(store);
    if was_cut {
        sim.crash();
    }
    let mut store = open(&mut sim, "store");
    let mut scan = None;
    for _ in 0..8 {
        scan = store.pins_take();
        if scan.is_some() {
            break;
        }
        flush(&mut sim, &mut store);
    }
    let mut scan = scan.ok_or("the pins could not be read in eight tries")?;
    scan.run();
    store.pins_put(scan);
    let root = store.pins().find(|pin| pin.name == "crate").ok_or("the pin is gone")?.keys[0];
    let handle = Handle::new(&mut store, "/sim".into());
    let kept = kept_by(&handle, root)?;
    let sets = Sets::new();
    sets.adopt(&handle, root)?;
    for n in 0..LIVE {
        if kept.get(&key(n)) != Some(&item(n)) {
            return Err(format!("item {n} is kept by nothing the pin's root names"));
        }
        let mut read = None;
        sets.get(&handle, &set, &key(n).0, &mut |bytes| read = Some(bytes.to_vec()))?;
        if read != Some(item(n)) {
            return Err(format!("item {n} does not read under its set"));
        }
    }
    if kept.len() as u64 != LIVE {
        return Err(format!("the root keeps {} items of a set of {LIVE}", kept.len()));
    }
    drop(handle);
    Ok(was_cut)
}

/// The invariant: at every point of a compaction, crashes included, the items of the set
/// are kept by the root the pin names, as records or as a sealed set, never as neither: so
/// a collector that keeps what a kept root names forgets none of them. The power goes after
/// each completion of the seal's commit and of the pin's, in turn, under four seeds.
#[test]
fn a_crash_at_any_point_of_a_compaction_leaves_the_records_or_the_sealed_set() {
    let mut cuts = 0;
    for seed in 0..4 {
        compacted(seed, None, false).unwrap_or_else(|why| panic!("seed {seed}, no crash: {why}"));
        for stage in 0..2 {
            for cut in 0.. {
                let was_cut = compacted(seed, Some((stage, cut)), false)
                    .unwrap_or_else(|why| panic!("seed {seed}, stage {stage}, cut after {cut}: {why}"));
                if !was_cut {
                    break;
                }
                cuts += 1;
            }
        }
    }
    assert!(cuts >= 4 * 2, "each commit has a completion to be cut after: {cuts} cuts");
}

/// The planted-bug control: a seal whose root leaves out the sealed set it made
/// (`SEALED_NOT_NAMED`). The oracle says so as soon as the pin names that root.
#[test]
fn a_root_that_leaves_out_its_sealed_set_is_caught() {
    let why = compacted(1, None, true).unwrap_err();
    assert!(why.contains("is kept by nothing the pin's root names"), "{why}");
    // Cut before the pin moved, the root before still keeps the records: no loss yet.
    assert_eq!(compacted(1, Some((0, 0)), true), Ok(true));
}

/// A set at [`sets::MAX_SEALED`] sealed sets is merged by its next seal into one, which
/// holds every item of them all; a member got again from its old record after a seal is
/// not listed as a record again.
#[test]
fn seals_past_the_bound_merge_into_one() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    let handle = Handle::new(&mut store, "/sim".into());
    let sets = Sets::new();
    let set = set_id(1);
    let mut last = None;
    for round in 0..=sets::MAX_SEALED as u64 {
        for n in round * 10..round * 10 + 10 {
            sets.put(&handle, &set, key(n).0, &item(n)).unwrap();
        }
        let sealing = sets.seal(&handle, &set).unwrap().unwrap();
        let merged = if round == sets::MAX_SEALED as u64 { round * 10 } else { 0 };
        assert_eq!((sealing.items, sealing.merged), (10 + merged, merged), "round {round}");
        last = Some(sealing);
    }
    let last = last.unwrap();
    let kept = kept_by(&handle, last.root).unwrap();
    assert_eq!(kept.len() as u64, 10 * (sets::MAX_SEALED as u64 + 1));
    // Its record is still in the store: got by its key, it is a member again, and the
    // listing leaves it to the sealed set.
    assert!(sets.get(&handle, &set, &key(3).0, &mut |_| ()).unwrap());
    let (roots, listed) = sets.list(&handle).unwrap();
    assert_eq!((listed.sets, listed.changed), (1, 0), "{listed:?}");
    assert_eq!(roots, vec![(set, last.root)]);
    assert_eq!(sets.quiet(1, 0), vec![set]);
    assert!(sets.quiet(2, 0).is_empty());
    drop(handle);
}
