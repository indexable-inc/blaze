//! `Lent`'s one promise (lent.rs, ruling 6): bytes handed out early stay byte-equal AND at the
//! same address after later gets that map more segments and after puts that add chunks. The
//! planted control is the arena as one growing buffer (`LentPlant::OneBuffer`): its first
//! reallocation moves what it lent, and the address check catches the move. The planted run
//! compares addresses only (a moved lend is never read).

use std::path::Path;

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};

use super::super::{
    core::{PlainConfig, TAGS},
    driven::Driven,
};
use super::{Lent, LentPlant};
use crate::Key;

const TAG: Tag = Tag(1);

fn cfg(n: u64, root: &blaze_mem::Frame) -> PlainConfig<&blaze_mem::Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce: 700 + n,
        durable: false,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain lent test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    (0..40 + n as usize % 300).map(|i| (n as usize).wrapping_mul(7).wrapping_add(i) as u8).collect()
}

/// Puts larger than a chunk: each takes a chunk of its own.
fn big(n: u32) -> Vec<u8> {
    vec![n as u8; (1 << 20) + 16]
}

/// Three committed segments (keys 0..10, 10..30, 30..60) and a lazy store over them, in a Lent.
fn lent(seed: u64, plant: LentPlant, root: &blaze_mem::Frame) -> Lent<Sim, &blaze_mem::Frame> {
    let mut w = Driven::with(Sim::new(seed, Faults::default()), Path::new("/sim"), cfg(0, root), root).expect("the writer opens");
    for range in [0..10u32, 10..30, 30..60] {
        for n in range {
            w.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        }
        w.commit_with(&[]).unwrap();
    }
    let r = Driven::with(w.into_driver(), Path::new("/sim"), cfg(1, root), root).expect("the reader opens");
    Lent::planted(r, root, plant)
}

/// The address of `n`'s bytes as `lent` lends them now.
fn address(lent: &Lent<Sim, &blaze_mem::Frame>, k: &Key) -> usize {
    lent.get(k).unwrap().expect("held").bytes.as_ptr() as usize
}

#[test]
fn lends_keep_their_address_and_bytes_across_maps_and_chunks() {
    let root = super::super::test_root();
    let lent = lent(21, LentPlant::None, &root);
    // Early lends: a small own put and a record of one segment.
    lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)).unwrap();
    let mut early = Vec::new();
    early.push((key(1000), lent.get(&key(1000)).unwrap().unwrap(), value(1000)));
    early.push((key(15), lent.get(&key(15)).unwrap().unwrap(), value(15)));
    let addresses: Vec<usize> = early.iter().map(|(_, lend, _)| lend.bytes.as_ptr() as usize).collect();
    // Later puts that add chunks, and gets that map the other segments.
    for n in 0..5 {
        lent.put(&key(2000 + n), Namespace::LOCAL, TAG, &big(n)).unwrap();
        early.push((key(2000 + n), lent.get(&key(2000 + n)).unwrap().unwrap(), big(n)));
    }
    for n in (0..10).chain(30..60) {
        early.push((key(n), lent.get(&key(n)).unwrap().unwrap(), value(n)));
    }
    let (mapped, named) = lent.mapped_of_named();
    assert_eq!((mapped, named), (3, 3));
    assert!(lent.counts().chunks >= 5, "the puts added fewer chunks than the test needs: {:?}", lent.counts());
    for (i, (k, lend, want)) in early.iter().enumerate() {
        assert_eq!(lend.bytes, &want[..], "lend {i} changed its bytes");
        assert_eq!(address(&lent, k), lend.bytes.as_ptr() as usize, "lend {i} moved");
    }
    assert_eq!(addresses, early[..2].iter().map(|(_, lend, _)| lend.bytes.as_ptr() as usize).collect::<Vec<_>>());
    println!("lent: {} lends kept their address and bytes across {mapped} maps and {} chunks", early.len(), lent.counts().chunks);
}

#[test]
fn the_planted_one_buffer_arena_moves_a_lend() {
    let root = super::super::test_root();
    let lent = lent(22, LentPlant::OneBuffer, &root);
    lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)).unwrap();
    let first = address(&lent, &key(1000));
    for n in 0..5 {
        lent.put(&key(2000 + n), Namespace::LOCAL, TAG, &big(n)).unwrap();
    }
    // Addresses only: the planted arena's first lend may be freed memory now.
    let now = address(&lent, &key(1000));
    assert_ne!(first, now, "the one growing buffer did not move: the plant was not exercised");
    println!("lent, planted OneBuffer: caught (a lent address moved)");
}

/// Equal bytes under one key are one value, whether the key is held by an earlier commit or by
/// this command's own put; other bytes, or another tag, are refused, naming both records'
/// lengths and the first differing byte. Prints
/// `lent, one key: equal bytes kept over two commits and the arena; refused at byte 3 (committed), at 140 (own), equal bytes of another tag refused`.
#[test]
fn equal_bytes_under_one_key_are_kept_and_other_bytes_refused_by_offset() {
    use super::LentError;
    use blaze_keyed::store::{Holder, Put, TwoValues};
    let root = super::super::test_root();
    let lent = lent(23, LentPlant::None, &root);
    // Committed by the first commit and by the second: equal bytes are the same value.
    for n in [5u32, 15] {
        assert!(matches!(lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)), Ok(Put::Same)), "equal bytes of committed key {n} refused");
    }
    let mut other = value(5);
    other[3] ^= 1;
    let refused = lent.put(&key(5), Namespace::LOCAL, TAG, &other).expect_err("other bytes under a committed key were kept");
    let LentError::TwoValues { two, .. } = refused else { panic!("not a two-values refusal: {refused}") };
    assert_eq!((two.len, two.held_len, two.held, two.differs_at), (Some(other.len() as u64), value(5).len() as u64, Holder::Committed, Some(3)), "{refused}");
    let words = std::format!("a committed record of kind {} and {} bytes, and a put of kind {} and {} bytes, first differing at byte 3", TAG.0, value(5).len(), TAG.0, other.len());
    assert!(refused.to_string().contains(words.as_str()), "{refused}");
    // This command's own put: once written, the same bytes again are the same value.
    assert!(matches!(lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)), Ok(Put::Written)));
    assert!(matches!(lent.put(&key(1000), Namespace::LOCAL, TAG, &value(1000)), Ok(Put::Same)));
    let mut longer = value(1000);
    let own_len = longer.len();
    longer.push(0);
    let refused = lent.put(&key(1000), Namespace::LOCAL, TAG, &longer).expect_err("a longer value under an own key was kept");
    let LentError::TwoValues { two, .. } = refused else { panic!("not a two-values refusal: {refused}") };
    assert_eq!((two.held, two.differs_at), (Holder::ThisProcess, Some(own_len as u64)), "{refused}");
    let refused = lent.put(&key(1000), Namespace::LOCAL, Tag(2), &value(1000)).expect_err("equal bytes of another tag were kept");
    let LentError::TwoValues { two: TwoValues { differs_at: None, .. }, .. } = refused else { panic!("equal bytes not said equal: {refused}") };
    println!("lent, one key: equal bytes kept over two commits and the arena; refused at byte 3 (committed), at {own_len} (own), equal bytes of another tag refused");
}

/// A put of parts is the put of their concatenation: the same bytes lent (split at every offset,
/// so a part ends inside a block and between blocks), the same value for a whole put of the
/// joined bytes, and a refusal of other parts naming the first differing byte of the whole.
#[test]
fn a_put_of_parts_is_the_put_of_their_concatenation() {
    use super::LentError;
    use blaze_keyed::store::Put;
    let root = super::super::test_root();
    let lent = lent(24, LentPlant::None, &root);
    for n in 2000u32..2040 {
        let whole = value(n);
        let at = (n as usize - 2000) % whole.len();
        let (head, tail) = whole.split_at(at);
        assert!(matches!(lent.put_parts(&key(n), Namespace::LOCAL, TAG, &[head, tail]), Ok(Put::Written)), "parts split at {at}");
        assert_eq!(lent.get(&key(n)).unwrap().expect("held").bytes, &whole[..], "the bytes lent are the parts joined (split at {at})");
        assert!(matches!(lent.put(&key(n), Namespace::LOCAL, TAG, &whole), Ok(Put::Same)), "the joined bytes are the same value (split at {at})");
        let mut other = whole.clone();
        other[whole.len() - 1] ^= 1;
        let (head, tail) = other.split_at(at);
        let refused = lent.put_parts(&key(n), Namespace::LOCAL, TAG, &[head, tail]).expect_err("other parts under a held key were kept");
        let LentError::TwoValues { two, .. } = refused else { panic!("not a two-values refusal: {refused}") };
        assert_eq!(two.differs_at, Some(whole.len() as u64 - 1), "{refused}");
    }
    // A committed key: parts of its bytes are the same value (the commit's segment).
    let whole = value(7);
    assert!(matches!(lent.put_parts(&key(7), Namespace::LOCAL, TAG, &[&whole[..5], &whole[5..]]), Ok(Put::Same)));
}

/// THE ARENA HOLDS NO PUT PAST ITS COMMIT (rule 51): puts no get lent from are freed by the
/// commit that writes them, and every later get reads them from the segment's mapping, byte-equal
/// (`own: false`); a put of the same bytes again is the same value, of other bytes the refusal.
/// A chunk a get lent from is kept with its lend standing (address and bytes), takes no more
/// puts, and holds no key after the commit (a later get of its key reads the segment). CONTROL: `LentPlant::FreesLent` (the commit frees a chunk under a live lend) leaves no
/// held bytes where the kept chunk must be (its lend is never read).
#[test]
fn a_commit_frees_every_put_no_get_lent() {
    use blaze_keyed::store::Put;
    let root = super::super::test_root();
    for plant in [LentPlant::None, LentPlant::FreesLent] {
        let lent = lent(25, plant, &root);
        // One put read before the commit (its chunk kept), then many puts no one read: a big one
        // in a chunk of its own, and small ones in a fresh chunk after the lent one.
        lent.put(&key(3000), Namespace::LOCAL, TAG, &value(3000)).unwrap();
        let early = lent.get(&key(3000)).unwrap().expect("held");
        lent.put(&key(3001), Namespace::LOCAL, TAG, &big(3001)).unwrap();
        for n in 3002u32..3100 {
            lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        }
        let before = lent.counts();
        assert!(before.held_bytes >= (big(3001).len() + 98 * 40) as u64, "the arena holds its puts before the commit: {before:?}");
        lent.commit().unwrap();
        let after = lent.counts();
        if plant == LentPlant::FreesLent {
            assert_eq!(after.held_bytes, 0, "planted: the lent chunk was freed too");
            continue;
        }
        // The lent chunk alone is kept: it holds key 3000 and nothing put after the lend.
        assert!(after.held_bytes > 0 && after.held_bytes <= 1 << 20, "only the lent chunk is held: {after:?}");
        assert!(after.freed_chunks >= 2, "the big put's chunk and the small puts' chunk freed: {after:?}");
        assert_eq!(early.bytes, &value(3000)[..], "the early lend stands");
        // The key lent before the commit is read from its segment after it, like every other
        // (aligned_tests.rs `a_get_before_the_commit_leaves_every_key_in_place_after_it`); the
        // early lend is still the kept chunk's bytes, at another address.
        let again = lent.get(&key(3000)).unwrap().expect("committed");
        assert!(!again.own && again.bytes == &value(3000)[..], "the key lent before the commit is read from its segment after it");
        assert_ne!(again.bytes.as_ptr(), early.bytes.as_ptr(), "the early lend is the kept chunk's, the later get the segment's");
        let gets = lent.counts().from_segments;
        for n in 3001u32..3100 {
            let want = if n == 3001 { big(n) } else { value(n) };
            let got = lent.get(&key(n)).unwrap().expect("committed");
            assert!(!got.own, "key {n} is read from the segment, not the arena");
            assert_eq!(got.bytes, &want[..], "key {n}'s bytes from the segment");
            assert!(matches!(lent.put(&key(n), Namespace::LOCAL, TAG, &want), Ok(Put::Same)), "key {n} again: the same value");
        }
        assert_eq!(lent.counts().from_segments - gets, 99, "every freed put read from its segment");
        let mut other = value(3050);
        other[0] ^= 1;
        assert!(lent.put(&key(3050), Namespace::LOCAL, TAG, &other).is_err(), "other bytes under a freed put's key: refused");
        // A put after the commit goes to a fresh chunk, never into the lent one.
        lent.put(&key(3100), Namespace::LOCAL, TAG, &value(3100)).unwrap();
        lent.commit().unwrap();
        assert_eq!(lent.counts().held_bytes, after.held_bytes, "the second commit frees the put after the first: {:?}", lent.counts());
        println!("lent: {} bytes held before the commit, {} after (the lent chunk), {} chunks freed", before.held_bytes, after.held_bytes, lent.counts().freed_chunks);
    }
}

fn reached_cfg(n: u64, cap_bytes: u64, root: &blaze_mem::Frame) -> PlainConfig<&blaze_mem::Frame> {
    PlainConfig { cap_bytes, nonce: 900 + n, liveness: super::super::Liveness::Reached, ..cfg(n, root) }
}

/// O(1) `renew_root` refreshes TTL without probing/re-hashing held names, preserves held names
/// past the original TTL, returns `Fresh` within `TOUCH_NS`, and returns `Missing` for absent or
/// expired roots.
#[test]
fn renew_root_refreshes_ttl_preserves_held_names_and_misses_absent_or_expired() {
    use core::time::Duration;
    use super::super::core::{GRACE_NS, LEASE_NS, ROOT_LIFE_NS, Renewed, TOUCH_NS};

    let root = super::super::test_root();
    let sim = Sim::new(401, Faults::default());
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(1, u64::MAX, &root), &root).expect("open");
    let k1 = key(4001);
    let k2 = key(4002);
    store.put(k1, Namespace::LOCAL, TAG, &value(4001)).expect("put k1");
    store.put(k2, Namespace::LOCAL, TAG, &value(4002)).expect("put k2");
    let root_key = store.root_for("answer", &[k1, k2], ROOT_LIFE_NS).expect("root_for");
    store.release_lease();
    let c1 = store.commit_with(&[]).expect("commit 1");
    let gen1 = c1.generation;
    let lent = Lent::new(store, &root);

    // Absent root -> Missing.
    assert_eq!(lent.renew_root("does-not-exist", ROOT_LIFE_NS), Renewed::Missing);

    // Within TOUCH_NS of creation -> Fresh, no commit flip, 0 extra root probes.
    let probes_before = lent.put_tally().disk.root_probes;
    assert_eq!(lent.renew_root("answer", ROOT_LIFE_NS), Renewed::Fresh(root_key));
    let c_noop = lent.commit().expect("noop commit");
    assert_eq!(c_noop.generation, gen1, "fresh renewal owes no flip");
    assert_eq!(lent.put_tally().disk.root_probes, probes_before, "renew_root probes 0 keys");

    // Advance past TOUCH_NS (5 days), still well within ROOT_LIFE_NS (14 days).
    let mut store = lent.into_store();
    store.driver().advance(Duration::from_nanos(TOUCH_NS * 5));
    let lent = Lent::new(store, &root);
    let probes_before = lent.put_tally().disk.root_probes;
    assert_eq!(lent.renew_root("answer", ROOT_LIFE_NS), Renewed::Extended(root_key));
    let c2 = lent.commit().expect("renew commit");
    assert!(c2.generation > gen1, "extended renewal flips manifest");
    assert!(c2.segments.is_empty(), "renew_root writes no new segment");
    assert_eq!(lent.put_tally().disk.root_probes, probes_before, "extended renewal probes 0 keys");

    // Advance past original expiry (T0 + 15 days), still before renewed expiry (T0 + 19 days).
    let mut store = lent.into_store();
    store.driver().advance(Duration::from_nanos(ROOT_LIFE_NS - TOUCH_NS * 4));
    let mut sim = store.into_driver();
    sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)));
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(2, u64::MAX, &root), &root).expect("reopen");
    store.fold_slice().expect("fold_slice");
    store.release_lease();
    store.commit_with(&[]).expect("commit after fold");
    assert!(store.get(&k1).expect("get k1").is_some(), "renewed root preserves k1 past original TTL");
    assert!(store.get(&k2).expect("get k2").is_some(), "renewed root preserves k2 past original TTL");
    assert_eq!(store.core().root_named("answer"), Some(root_key), "root still live");

    // Advance past renewed expiry -> renew_root returns Missing (never resurrects expired root).
    store.driver().advance(Duration::from_nanos(ROOT_LIFE_NS));
    assert_eq!(store.renew_root("answer", ROOT_LIFE_NS), Renewed::Missing);
}

/// Lazy `live_roots` in `Plain::reused`: commits with brand-new keys succeed without scanning
/// existing roots, while commits reusing an unrooted committed key still detect and protect it.
#[test]
fn reused_handles_fresh_and_reused_committed_keys_identically() {
    use core::time::Duration;
    use super::super::core::{LEASE_NS, ROOT_LIFE_NS};

    let root = super::super::test_root();
    let sim = Sim::new(402, Faults::default());
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(1, u64::MAX, &root), &root).expect("open");

    // Commit an existing indexed root and an unrooted committed record.
    let existing_k = key(5001);
    let unrooted_k = key(5002);
    store.put(existing_k, Namespace::LOCAL, TAG, &value(5001)).expect("put existing");
    store.put(unrooted_k, Namespace::LOCAL, TAG, &value(5002)).expect("put unrooted");
    let idx = store.index(&[existing_k]).expect("index");
    store.root_through("existing", &[idx], ROOT_LIFE_NS).expect("root_through");
    store.release_lease();
    store.commit_with(&[]).expect("commit 1");

    // 1. Fresh-only commit (like a 2-key journal checkpoint): zero collisions with existing segments.
    let fresh_k = key(5003);
    store.put(fresh_k, Namespace::LOCAL, TAG, &value(5003)).expect("put fresh");
    store.root_for("checkpoint", &[fresh_k], ROOT_LIFE_NS).expect("root checkpoint");
    store.commit_with(&[]).expect("commit fresh");
    let found = store.get(&fresh_k).expect("get").expect("present");
    assert_eq!(store.got(found).bytes, &value(5003)[..]);

    // 2. Commit that reuses the unrooted committed key `unrooted_k`: `reused` lazily loads
    // `live_roots` on the `find_in` hit, finds it unreached, and keeps it intact across sweep.
    let sim = store.into_driver();
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(2, u64::MAX, &root), &root).expect("reopen");
    store.root_for("reuser", &[unrooted_k], ROOT_LIFE_NS).expect("root reuser");
    store.commit_with(&[]).expect("commit reuser");

    store.driver().advance(Duration::from_nanos(LEASE_NS) + Duration::from_secs(1));
    store.fold_slice().expect("fold");
    store.release_lease();
    store.commit_with(&[]).expect("commit after fold");
    let found = store.get(&unrooted_k).expect("get").expect("present");
    assert_eq!(store.got(found).bytes, &value(5002)[..]);
    let found = store.get(&existing_k).expect("get").expect("present");
    assert_eq!(store.got(found).bytes, &value(5001)[..]);
    let found = store.get(&fresh_k).expect("get").expect("present");
    assert_eq!(store.got(found).bytes, &value(5003)[..]);
}

/// Many unexpired roots sharing many indexes stay O(unique keys + roots) in `Plain::reused` and
/// `compute_reach` (no O(keys × roots × indexes) blowup), while still protecting an unrooted
/// committed key reused through a new index.
#[test]
fn reused_dedups_shared_indexes_across_many_roots() {
    use core::time::Duration;
    use super::super::core::{LEASE_NS, ROOT_LIFE_NS};

    let root = super::super::test_root();
    let sim = Sim::new(404, Faults::default());
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(1, u64::MAX, &root), &root).expect("open");

    const INDEXES: usize = 32;
    const KEYS_PER_INDEX: usize = 64;
    let mut shared_indexes = [key(0); INDEXES];
    for (idx_i, slot) in shared_indexes.iter_mut().enumerate() {
        let mut keys = [key(0); KEYS_PER_INDEX];
        for (key_j, key_slot) in keys.iter_mut().enumerate() {
            let id = 10_000 + (idx_i * KEYS_PER_INDEX + key_j) as u32;
            let k = key(id);
            store.put(k, Namespace::LOCAL, TAG, &value(id)).expect("put");
            *key_slot = k;
        }
        *slot = store.index(&keys).expect("index");
    }
    let unrooted_k = key(99_999);
    store.put(unrooted_k, Namespace::LOCAL, TAG, &value(99_999)).expect("put unrooted");
    for r in 0..32u32 {
        store.root_through(&format!("shared-{r}"), &shared_indexes, ROOT_LIFE_NS).expect("root_through");
    }
    store.release_lease();
    store.commit_with(&[]).expect("commit 1");

    // Reopen and commit a new indexed root sharing all 32 indexes plus a new index reusing `unrooted_k`.
    let sim = store.into_driver();
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(2, u64::MAX, &root), &root).expect("reopen");
    let extra_idx = store.index(&[unrooted_k]).expect("extra index");
    let mut new_indexes = [key(0); INDEXES + 1];
    new_indexes[..INDEXES].copy_from_slice(&shared_indexes);
    new_indexes[INDEXES] = extra_idx;
    store.root_through("new-shared", &new_indexes, ROOT_LIFE_NS).expect("root_through new");
    store.commit_with(&[]).expect("commit new");

    // Advance past lease and sweep: both shared keys and the reused unrooted key survive.
    store.driver().advance(Duration::from_nanos(LEASE_NS) + Duration::from_secs(1));
    store.fold_slice().expect("fold");
    store.release_lease();
    store.commit_with(&[]).expect("commit after fold");
    let found = store.get(&unrooted_k).expect("get unrooted").expect("present");
    assert_eq!(store.got(found).bytes, &value(99_999)[..]);
    let sample = key(10_000);
    let found = store.get(&sample).expect("get sample").expect("present");
    assert_eq!(store.got(found).bytes, &value(10_000)[..]);
}

/// `needs_housekeeping` is `false` on a clean bounded store and `true` in each trigger condition:
/// total bytes over cap, expired root, tier count exceeding `K`, and expired retired file.
#[test]
fn needs_housekeeping_clean_and_each_trigger_condition() {
    use super::super::core::{GRACE_NS, ROOT_LIFE_NS, TIER_SEGMENTS_MOST};

    let root = super::super::test_root();
    let sim = Sim::new(403, Faults::default());
    let mut store = Driven::with(sim, Path::new("/sim"), reached_cfg(1, 10 << 20, &root), &root).expect("open");

    let k0 = key(6000);
    store.put(k0, Namespace::LOCAL, TAG, &value(6000)).expect("put k0");
    store.root_for("keep", &[k0], ROOT_LIFE_NS).expect("root keep");
    store.release_lease();
    store.commit_with(&[]).expect("commit 1");
    let now = store.driver().wall_ns();

    // Clean bounded store -> false.
    assert!(!store.needs_housekeeping(now), "clean store with 1 segment needs no housekeeping");

    // Trigger 1: total bytes over cap -> true; back under cap -> false.
    store.core_mut().set_cap(1);
    assert!(store.needs_housekeeping(now), "over cap triggers housekeeping");
    store.core_mut().set_cap(10 << 20);
    assert!(!store.needs_housekeeping(now), "back under cap clears housekeeping");

    // Trigger 2: expired root -> true.
    let expires = store.core().roots_expiring()[0].2;
    assert!(!store.needs_housekeeping(expires - 1), "unexpired root does not trigger");
    assert!(store.needs_housekeeping(expires), "expired root triggers housekeeping");

    // Trigger 3: tier exceeds K (TIER_SEGMENTS_MOST = 4).
    let mut keys = [k0; TIER_SEGMENTS_MOST + 1];
    for i in 1..=TIER_SEGMENTS_MOST {
        let k = key(6000 + i as u32);
        keys[i] = k;
        store.put(k, Namespace::LOCAL, TAG, &value(6000 + i as u32)).expect("put");
        store.root_for("keep", &keys[..=i], ROOT_LIFE_NS).expect("root");
        store.release_lease();
        store.commit_with(&[]).expect("commit");
        let t = store.driver().wall_ns();
        let segs = store.core().segments().len();
        if segs <= TIER_SEGMENTS_MOST {
            assert!(!store.needs_housekeeping(t), "{segs} segments in tier 0 (<= K) needs no housekeeping");
        } else {
            assert!(store.needs_housekeeping(t), "{segs} segments in tier 0 (> K) triggers housekeeping");
        }
    }

    // Compact merges tier 0 back to 1 segment and retires the old ones.
    store.compact(1).expect("compact");
    let after_compact = store.driver().wall_ns();
    assert!(!store.needs_housekeeping(after_compact), "freshly compacted store (retired within grace) needs no housekeeping");

    // Trigger 4: expired retired file past GRACE_NS -> true.
    assert!(store.needs_housekeeping(after_compact + GRACE_NS), "retired file past grace triggers housekeeping");
}
