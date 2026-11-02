use std::{collections::BTreeMap, time::Duration};

use blaze_core::{
    Driver, run_until_idle,
    sim::{Faults, Rng, Sim},
};

use super::*;
use crate::read::Found;

mod model_check;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config(dir: &str) -> Config {
    Config { dir: name(dir), layers: Vec::new(), tags: 1 << 40 }
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 store test key 1", &n.to_le_bytes()))
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Bytes of a run's output: alike between runs, as the outputs of build scripts are.
fn run_output(n: u64) -> Vec<u8> {
    format!(
        "cargo:rustc-link-lib=static=crate{n}\ncargo:rustc-cfg=has_feature_{}\n\
         cargo:rerun-if-changed=src/native/{n}.c\ncargo:rustc-env=BUILT_AT=/build/{n}/out\n\
         cargo:warning=compiled {} objects\n",
        n % 13,
        n * 31 % 97
    )
    .repeat(6)
    .into_bytes()
}

/// Runs everything asked for, handing the store its completions; returns the tickets made
/// durable.
fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Vec<Ticket> {
    let mut durable = Vec::new();
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            durable.extend(store.complete(completion, effects));
        }
    });
    durable
}

fn open_with(sim: &mut Sim, config: Config) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open(config, &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

fn open(sim: &mut Sim, dir: &str) -> Store {
    open_with(sim, config(dir))
}

fn flush(sim: &mut Sim, store: &mut Store) -> Vec<Ticket> {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(sim, store, &mut effects)
}

/// `len` bytes of text that compresses, different for each `n`.
fn text(n: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 512);
    let mut at = n * 1000;
    while out.len() < len {
        out.extend(run_output(at));
        at += 1;
    }
    out.truncate(len);
    out
}

fn settle(sim: &mut Sim, store: &mut Store) -> Settled {
    let mut effects = Vec::new();
    let settled = store.settle(&mut effects);
    drive(sim, store, &mut effects);
    settled
}

impl Store {
    /// What a lookup of `key` finds; it must be there and whole.
    fn found_of(&self, key: &Key) -> Found<'_> {
        self.look().find(key).expect("the store has the key").expect("its record is whole")
    }

    /// Where the record of `key` is in its pack: once a commit has moved it out of its segment.
    fn loc_of(&self, key: &Key) -> Loc {
        match self.found_of(key).place {
            Place::Pack(loc) => loc,
            Place::Segment(_) => panic!("{key:?} is still in its segment"),
        }
    }

    /// The form of the record of `key`, and the bytes it stores and holds, wherever it is.
    fn sizes_of(&self, key: &Key) -> (Form, u64, u64) {
        match self.found_of(key).place {
            Place::Pack(loc) => (loc.form, loc.stored, loc.raw),
            Place::Segment(seg) => (seg.form, seg.stored, seg.raw),
        }
    }

    fn flag_of(&self, key: &Key, flag: Flag) -> bool {
        self.found_of(key).at.get(flag)
    }

    fn delta_len(&self) -> usize {
        self.tiers[0].delta.len()
    }
}

fn form_of(store: &Store, key: &Key) -> Form {
    store.found_of(key).place.form()
}

fn quiet() -> Faults {
    Faults::default()
}

fn bytes_of(store: &Store, key: &Key) -> Option<Vec<u8>> {
    store.get(key).unwrap().map(|it| it.bytes.to_vec())
}

#[test]
fn a_put_is_read_at_once_durable_after_a_flush_and_there_after_reopening() {
    let mut sim = Sim::new(1, quiet());
    let mut store = open(&mut sim, "store");
    let values: Vec<(Key, Kind, Vec<u8>)> = vec![
        (key(1), Kind::Query, b"a query's result".to_vec()),
        (key(2), Kind::Code, noise(5000, 2)),
        (key(3), Kind::Debug, b"debug info".to_vec()),
        (key(4), Kind::Run, run_output(4)),
        (key(5), Kind::Output, noise(1000, 5)),
        (key(6), Kind::Image, Vec::new()),
    ];
    let mut tickets = Vec::new();
    for (key, kind, bytes) in &values {
        tickets.push(store.put(*key, *kind, bytes.clone()).unwrap());
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
        assert_eq!(entry.content, content_of(bytes));
    }
    assert!(tickets.iter().all(|it| !store.durable(*it)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable, tickets);
    assert!(tickets.iter().all(|it| store.durable(*it)));
    for (key, _, bytes) in &values {
        assert_eq!(bytes_of(&store, key).as_ref(), Some(bytes));
    }
    drop(store);
    let store = open(&mut sim, "store");
    for (key, kind, bytes) in &values {
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
    }
    assert!(store.get(&key(99)).unwrap().is_none());
    assert!(!store.has(&key(99)));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_second_put_keeps_the_first_and_other_bytes_are_a_mismatch() {
    let mut sim = Sim::new(2, quiet());
    let mut store = open(&mut sim, "store");
    let first = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    let again = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    assert!(again >= first);
    let mismatch = store.put(key(1), Kind::Query, b"two".to_vec()).unwrap_err();
    assert_eq!(mismatch.kept, content_of(b"one"));
    assert_eq!(mismatch.given, content_of(b"two"));
    flush(&mut sim, &mut store);
    let durable_again = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    assert!(store.durable(durable_again), "a value already durable needs no wait");
    assert!(store.put(key(1), Kind::Query, b"two".to_vec()).is_err());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
}

/// A put of what a commit in flight already writes is durable with that commit.
#[test]
fn a_put_of_a_value_in_flight_is_durable_with_its_commit() {
    let mut sim = Sim::new(3, quiet());
    let mut store = open(&mut sim, "store");
    let ticket = store.put(key(1), Kind::Code, b"code".to_vec()).unwrap();
    let mut effects = Vec::new();
    store.flush(&mut effects);
    let again = store.put(key(1), Kind::Code, b"code".to_vec()).unwrap();
    let durable = drive(&mut sim, &mut store, &mut effects);
    assert!(durable.contains(&ticket));
    assert!(store.durable(again));
}

/// The crash test: at any point of any number of commits, under injected IO failures, a crash
/// loses no put that was reported durable, and the store opens with only whole values.
#[test]
fn a_crash_at_any_point_loses_nothing_durable_and_serves_nothing_torn() {
    for seed in 0..120 {
        let mut sim = Sim::new(seed, Faults { fail_one_in: 9, ..Faults::default() });
        let mut expected: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut durable_keys: Vec<Key> = Vec::new();
        let mut next = 0u64;
        for round in 0..4 {
            let mut store = open(&mut sim, "store");
            for (key, bytes) in &expected {
                if let Some(found) = bytes_of(&store, key) {
                    assert_eq!(&found, bytes, "seed {seed} round {round}");
                }
            }
            for key in &durable_keys {
                assert!(store.has(key), "seed {seed} round {round}: a durable put was lost");
            }
            assert_eq!(store.verify(None), Vec::new(), "seed {seed} round {round}");
            let mut tickets = BTreeMap::new();
            let count = 5 + sim.rng().below(40);
            for _ in 0..count {
                next += 1;
                let (kind, bytes) = match next % 5 {
                    0 => (Kind::Run, run_output(next)),
                    1 => (Kind::Code, noise(200 + (next as usize * 37) % 3000, next)),
                    2 if seed % 4 == 0 => (Kind::Output, noise(300 << 10, next)),
                    3 if seed % 4 == 1 => (Kind::Query, noise(100 << 10, next)),
                    4 if seed % 3 == 2 => (Kind::Code, text(next, 6 << 10)),
                    _ => (Kind::Query, noise(64, next)),
                };
                let ticket = store.put(key(next), kind, bytes.clone()).unwrap();
                tickets.insert(ticket, key(next));
                expected.insert(key(next), bytes);
            }
            let mut effects = Vec::new();
            if seed % 3 == 2 {
                for _ in 0..2 {
                    store.settle(&mut effects);
                    for ticket in drive(&mut sim, &mut store, &mut effects) {
                        durable_keys.extend(tickets.get(&ticket).copied());
                    }
                }
                store.settle(&mut effects);
            }
            store.flush(&mut effects);
            let polls = sim.rng().below(40);
            for _ in 0..polls {
                sim.submit(&mut effects);
                let mut out = Vec::new();
                sim.poll(&mut out, Duration::ZERO);
                for completion in out {
                    if store.owns(completion.tag) {
                        for ticket in store.complete(completion, &mut effects) {
                            durable_keys.extend(tickets.get(&ticket).copied());
                        }
                    }
                }
            }
            if let Some(broken) = store.broken() {
                // Its commits met an injected failure `FAILURES_MOST` times in a row: it stops, as
                // on a failing disk, and the next round checks it lost nothing durable.
                assert!(
                    broken.what == "injected by the simulation" && store.failures >= FAILURES_MOST,
                    "seed {seed}: {broken:?} after {} failures in a row",
                    store.failures
                );
            }
            sim.crash();
        }
    }
}

#[test]
fn a_flipped_byte_of_a_value_is_never_served() {
    let mut sim = Sim::new(4, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, vec![b'a'; 1000]).unwrap();
    store.put(key(2), Kind::Query, vec![b'b'; 1000]).unwrap();
    flush(&mut sim, &mut store);
    let loc = store.loc_of(&key(2));
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[(loc.offset + RECORD_HEADER) as usize + 500] ^= 1;
    sim.change(&pack, &bytes);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), vec![b'a'; 1000]);
    let corrupt = store.get(&key(2)).unwrap_err();
    assert_eq!(corrupt.pack, pack);
    assert!(corrupt.what.contains("hash"), "{corrupt}");
    let found = store.verify(None);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, key(2));
    assert_eq!(store.verify(Some(Kind::Code)), Vec::new());
}

#[test]
fn a_record_header_that_is_not_whole_stops_the_store_naming_its_pack() {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, vec![7; 100]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[PACK_HEADER as usize + 40] ^= 0xff;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    assert_eq!(store.broken().unwrap().file, pack);
}

#[test]
fn a_head_of_another_format_is_refused_by_name() {
    let mut sim = Sim::new(6, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, vec![1]).unwrap();
    flush(&mut sim, &mut store);
    let head = name("store/head");
    let mut bytes = sim.contents(&head);
    bytes[8] = 99;
    sim.change(&head, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().unwrap();
    assert_eq!(broken.file, head);
    assert!(broken.what.contains("format 99"), "{broken}");
}

#[test]
fn near_identical_outputs_share_their_chunks() {
    let mut sim = Sim::new(7, quiet());
    let mut store = open(&mut sim, "store");
    let output_bytes = |store: &Store| -> u64 {
        store.tiers[0].packs.iter().map(|it| it.end).sum()
    };
    let first = noise(2 << 20, 70);
    let mut second = first.clone();
    second.splice(5000..5000, *b"a patched function").for_each(drop);
    second[1 << 20] ^= 1;
    store.put(key(1), Kind::Output, first.clone()).unwrap();
    flush(&mut sim, &mut store);
    let after_first = output_bytes(&store);
    store.put(key(2), Kind::Output, second.clone()).unwrap();
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), first);
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second, "read from its segment");
    flush(&mut sim, &mut store);
    let added = output_bytes(&store) - after_first;
    assert!(added * 4 < first.len() as u64, "the second output added {added} bytes");
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second);
    assert_eq!(store.verify(Some(Kind::Output)), Vec::new());
}

#[test]
fn a_cold_kind_trains_its_dictionary_and_compresses_what_follows() {
    let mut sim = Sim::new(8, quiet());
    let mut store = open(&mut sim, "store");
    let count = cold::TRAIN_VALUES as u64 + 40;
    for n in 0..count {
        store.put(key(n), Kind::Run, run_output(n)).unwrap();
    }
    assert_eq!(store.problems(), &[] as &[String]);
    assert_eq!(store.tiers[0].dictionaries.len(), 1, "one dictionary for runs");
    let (form, stored, raw) = store.sizes_of(&key(count - 1));
    assert_eq!(form, Form::Zstd);
    assert!(stored * 3 < raw, "{stored} stored of {raw}");
    assert_eq!(form_of(&store, &key(0)), Form::Raw);
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n));
    }
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_large_value_is_a_file_of_its_own_placed_by_sharing_blocks() {
    let mut sim = Sim::new(9, quiet());
    let mut store = open(&mut sim, "store");
    let big = noise(EXTERNAL_FROM + 10, 90);
    store.put(key(1), Kind::Image, big.clone()).unwrap();
    store.put(key(2), Kind::Code, b"small".to_vec()).unwrap();
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
    flush(&mut sim, &mut store);
    let file = name(&format!("store/{}", disk::external_file(&content_of(&big))));
    assert_eq!(sim.contents(&file), big);
    let mut effects = Vec::new();
    let a = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    let b = store.place(&key(2), name("target/small"), &mut effects).unwrap();
    assert_eq!(store.place(&key(3), name("target/none"), &mut effects), None);
    drive(&mut sim, &mut store, &mut effects);
    let placed: BTreeMap<Tag, Result<(), String>> =
        store.placed().into_iter().map(|it| (it.tag, it.result)).collect();
    assert_eq!(placed[&a], Ok(()));
    assert_eq!(placed[&b], Ok(()));
    assert_eq!(sim.contents(&name("target/image")), big);
    assert_eq!(sim.contents(&name("target/small")), b"small");
    // Placing again over what is there replaces it.
    let again = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.placed()[0], Placed { tag: again, to: name("target/image"), result: Ok(()) });
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
}

#[test]
fn where_blocks_cannot_be_shared_a_place_fails_and_copies_nothing() {
    let mut sim = Sim::new(10, Faults { no_clone: true, ..Faults::default() });
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, noise(EXTERNAL_FROM, 100)).unwrap();
    flush(&mut sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("out/q"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert!(matches!(&placed[0].result, Err(why) if why.contains("does not copy")), "{placed:?}");
    assert!(!sim.exists(&name("out/q")));
}

#[test]
fn a_layer_is_read_after_the_store_and_never_written() {
    let mut sim = Sim::new(11, quiet());
    let mut base = open(&mut sim, "base");
    base.put(key(1), Kind::Code, b"from the base".to_vec()).unwrap();
    base.put(key(2), Kind::Output, noise(400 << 10, 2)).unwrap();
    flush(&mut sim, &mut base);
    drop(base);
    let before = sim.contents(&name("base/head"));
    let mut store = open_with(&mut sim, Config {
        dir: name("mine"),
        layers: vec![name("base")],
        tags: 1 << 40,
    });
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"from the base");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), noise(400 << 10, 2));
    let ticket = store.put(key(1), Kind::Code, b"from the base".to_vec()).unwrap();
    assert!(store.durable(ticket));
    assert!(store.put(key(1), Kind::Code, b"other".to_vec()).is_err());
    store.put(key(3), Kind::Code, b"mine".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(sim.contents(&name("base/head")), before);
    assert!(sim.exists(&name("mine/head")));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_missing_layer_stops_the_store() {
    let mut sim = Sim::new(12, quiet());
    let mut effects = Vec::new();
    let mut store = Store::open(
        Config { dir: name("mine"), layers: vec![name("nowhere")], tags: 0 },
        &mut effects,
    );
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.broken().unwrap().file, name("nowhere/head"));
}

fn writer(n: u64) -> ContentId {
    ContentId::of("blaze v2 store test writer 1", &n.to_le_bytes())
}

/// Writer `n` of the shared directory `shared`, with tags of its own.
fn shared(n: u64) -> SharedConfig {
    SharedConfig { root: name("shared"), writer: writer(n), tags: (1 << 40) + (n << 33) }
}

fn writer_file(n: u64, file: &str) -> FileName {
    name(&format!("shared/w-{:x}/{file}", writer(n)))
}

fn open_shared(sim: &mut Sim, n: u64) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open_shared(shared(n), &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

/// Runs everything asked for, handing each store its completions.
fn drive_all(sim: &mut Sim, stores: &mut [&mut Store], effects: &mut Vec<Effect>) {
    run_until_idle(sim, effects, |completion, effects| {
        if let Some(store) = stores.iter_mut().find(|it| it.owns(completion.tag)) {
            store.complete(completion, effects);
        }
    });
}

/// A get as a daemon makes it of a shared store: a miss looks again, and gets once that look
/// is in.
fn get_after_miss(sim: &mut Sim, store: &mut Store, key: &Key) -> Option<Vec<u8>> {
    if let Some(bytes) = bytes_of(store, key) {
        return Some(bytes);
    }
    let mut effects = Vec::new();
    let look = store.look_again(&mut effects);
    drive(sim, store, &mut effects);
    assert!(store.looked() >= look, "{:?}", store.broken());
    bytes_of(store, key)
}

#[test]
fn two_processes_committing_at_once_lose_neither_commit() {
    let mut sim = Sim::new(13, quiet());
    let mut a = open_shared(&mut sim, 1);
    let mut b = open_shared(&mut sim, 2);
    for n in 0..60 {
        a.put(key(n), Kind::Code, text(n, 300)).unwrap();
        b.put(key(1000 + n), Kind::Code, text(1000 + n, 300)).unwrap();
    }
    // One key with its one bytes, put by both before either sees the other's.
    a.put(key(5000), Kind::Output, text(5000, 300)).unwrap();
    b.put(key(5000), Kind::Output, text(5000, 300)).unwrap();
    let mut effects = Vec::new();
    a.flush(&mut effects);
    b.flush(&mut effects);
    drive_all(&mut sim, &mut [&mut a, &mut b], &mut effects);
    assert!(a.broken().is_none() && b.broken().is_none(), "{:?} {:?}", a.broken(), b.broken());
    drop((a, b));
    let a = open_shared(&mut sim, 1);
    let c = open_shared(&mut sim, 3);
    for store in [&a, &c] {
        for n in (0..60).chain(1000..1060).chain([5000]) {
            assert_eq!(bytes_of(store, &key(n)), Some(text(n, 300)), "{n}");
        }
        assert_eq!(store.verify(None), Vec::new());
    }
}

#[test]
fn a_reader_sees_the_others_commit_after_a_miss() {
    let mut sim = Sim::new(14, quiet());
    let mut a = open_shared(&mut sim, 1);
    let mut b = open_shared(&mut sim, 2);
    b.put(key(1), Kind::Code, b"from b".to_vec()).unwrap();
    flush(&mut sim, &mut b);
    assert_eq!(bytes_of(&a, &key(1)), None, "not before it looks");
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(1)).unwrap(), b"from b");
    // A writer that came after it opened, and one that moved on past a new index table.
    let mut d = open_shared(&mut sim, 4);
    d.put(key(2), Kind::Code, b"from d".to_vec()).unwrap();
    flush(&mut sim, &mut d);
    for n in 10..70 {
        b.put(key(n), Kind::Query, text(n, 200)).unwrap();
    }
    flush(&mut sim, &mut b);
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(2)).unwrap(), b"from d");
    for n in 10..70 {
        assert_eq!(get_after_miss(&mut sim, &mut a, &key(n)), Some(text(n, 200)), "{n}");
    }
    assert_eq!(bytes_of(&a, &key(1)).unwrap(), b"from b");
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(9999)), None);
    assert_eq!(a.verify(None), Vec::new());
}

#[test]
fn a_value_another_writer_committed_is_one_entry() {
    let mut sim = Sim::new(15, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Code, b"made once".to_vec()).unwrap();
    flush(&mut sim, &mut a);
    let mut b = open_shared(&mut sim, 2);
    let ticket = b.put(key(1), Kind::Code, b"made once".to_vec()).unwrap();
    assert!(b.durable(ticket));
    assert_ne!(b.found_of(&key(1)).tier, 0);
    assert!(b.put(key(1), Kind::Code, b"other".to_vec()).is_err());
    flush(&mut sim, &mut b);
    assert!(!sim.exists(&writer_file(2, "pack-000000")));
}

#[test]
fn a_damaged_value_in_another_writers_store_is_never_served() {
    let mut sim = Sim::new(16, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Query, vec![b'a'; 1000]).unwrap();
    a.put(key(2), Kind::Query, vec![b'b'; 1000]).unwrap();
    flush(&mut sim, &mut a);
    let loc = a.loc_of(&key(2));
    drop(a);
    let pack = writer_file(1, "pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[(loc.offset + RECORD_HEADER) as usize + 500] ^= 1;
    sim.change(&pack, &bytes);
    let b = open_shared(&mut sim, 2);
    assert_eq!(bytes_of(&b, &key(1)).unwrap(), vec![b'a'; 1000]);
    let corrupt = b.get(&key(2)).unwrap_err();
    assert_eq!(corrupt.pack, pack);
    assert!(corrupt.what.contains("hash"), "{corrupt}");
}

#[test]
fn another_writers_head_that_cannot_be_read_stops_the_store_naming_it() {
    let mut sim = Sim::new(17, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Code, b"from a".to_vec()).unwrap();
    flush(&mut sim, &mut a);
    drop(a);
    let head = writer_file(1, HEAD_FILE);
    sim.change(&head, b"not a head");
    let mut effects = Vec::new();
    let mut b = Store::open_shared(shared(2), &mut effects);
    drive(&mut sim, &mut b, &mut effects);
    assert!(!b.ready());
    assert_eq!(b.broken().unwrap().file, head);
}

#[test]
fn a_shared_directory_others_can_write_to_is_refused() {
    let mut sim = Sim::new(18, quiet());
    sim.set_access(&name("shared"), blaze_core::Access::OthersWrite);
    let mut effects = Vec::new();
    let mut store = Store::open_shared(shared(1), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    assert_eq!(store.broken().unwrap().file, name("shared"));
}

/// Opened, and refused with what of it is not the user's alone and why; nothing in it is read.
fn refused(sim: &mut Sim) -> Broken {
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let mut asked = Vec::new();
    run_until_idle(sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        asked.extend(effects.iter().map(|it| format!("{it:?}")));
    });
    assert!(!store.ready());
    assert_eq!(asked, Vec::<String>::new(), "nothing is read from a store that is refused");
    let Broken { file, what } = store.broken().expect("refused");
    Broken { file: file.clone(), what: what.clone() }
}

#[test]
fn a_store_of_another_user_is_refused() {
    let mut sim = Sim::new(40, quiet());
    sim.set_access(&name("store"), blaze_core::Access::OtherUser);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, name("store"));
    assert!(broken.what.contains("belongs to another user"), "{broken}");
}

#[test]
fn a_store_others_can_write_to_is_refused() {
    let mut sim = Sim::new(41, quiet());
    sim.set_access(&name("store"), blaze_core::Access::OthersWrite);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, name("store"));
    assert!(broken.what.contains("can be written to by other users"), "{broken}");
}

/// A value is checked against its own hash and never against what its key says made it, so a
/// pack another user rewrote is refused before any of it is read.
#[test]
fn a_pack_others_can_write_to_is_not_read() {
    let mut sim = Sim::new(42, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"code".to_vec()).unwrap();
    store.put(key(2), Kind::Query, b"a result".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    assert!(sim.exists(&pack));
    sim.chmod(&pack, 0o646);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("can be written to by other users"), "{broken}");
    sim.chmod(&pack, 0o644);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), b"a result");
}

/// On this machine's disk: the store's directory is made the user's alone, closed to others if
/// it was open to their reading, and refused with a file in it others may write to.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_store_and_its_files_are_the_users_alone() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let chmod = |path: &std::path::Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let root = std::env::temp_dir().join(format!("blaze-cas-private-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    let dir = root.join("store");
    {
        let mut driven = Driven::open(&root).unwrap();
        driven.store().put(key(1), Kind::Query, b"kept".to_vec()).unwrap();
        driven.flush().unwrap();
    }
    assert_eq!(mode(&dir), 0o700);
    chmod(&dir, 0o705);
    drop(Driven::open(&root).unwrap());
    assert_eq!(mode(&dir), 0o700, "closed to others' reading");
    let head = dir.join("head");
    chmod(&head, 0o666);
    let refused = Driven::open(&root).err().unwrap();
    assert!(refused.contains("store/head") && refused.contains("can be written to by other users"), "{refused}");
    assert_eq!(mode(&head), 0o666, "what is refused is never mended");
    chmod(&dir, 0o777);
    let refused = Driven::open(&root).err().unwrap();
    assert!(refused.contains("store: can be written to by other users"), "{refused}");
    _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_index_is_rewritten_into_the_other_slot_and_the_head_flipped() {
    let mut sim = Sim::new(13, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..INDEX_AFTER as u64 + 5 {
        store.put(key(n), Kind::Query, n.to_le_bytes().to_vec()).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.tiers[0].head.index.unwrap().slot, 0);
    assert_eq!(store.delta_len(), 0, "the new table holds every record");
    for n in 1000..1000 + INDEX_AFTER as u64 {
        store.put(key(n), Kind::Code, n.to_le_bytes().to_vec()).unwrap();
    }
    flush(&mut sim, &mut store);
    let index = store.tiers[0].head.index.unwrap();
    assert_eq!(index.slot, 1);
    assert_eq!(index.count, 2 * INDEX_AFTER as u64 + 5);
    store.put(key(5000), Kind::Code, b"after".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.delta_len(), 1, "only what came after the table is read at open");
    for n in (0..INDEX_AFTER as u64 + 5).chain(1000..1000 + INDEX_AFTER as u64) {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), n.to_le_bytes(), "{n}");
    }
    assert_eq!(bytes_of(&store, &key(5000)).unwrap(), b"after");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn packs_roll_over_when_full() {
    let mut sim = Sim::new(14, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..80 {
        store.put(key(n), Kind::Code, noise(10 << 10, n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let packs = store.tiers[0].packs.len();
    assert!(packs >= 3, "{packs} packs");
    assert!(store.tiers[0].packs.iter().all(|it| it.end <= PACK_MOST));
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..80 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), noise(10 << 10, n));
    }
}

#[test]
fn a_head_round_trips_and_refuses_any_change() {
    let head = Head {
        generation: 7,
        index: Some(IndexRef { slot: 1, len: 64 + 1024 * 64, check: 0xdead_beef, count: 3 }),
        packs: vec![
            PackRef { unit: 0, committed: 4096, indexed: 16, dead: 0, removed: true },
            PackRef {
                unit: 2,
                committed: 3 * 4096,
                indexed: 4096,
                dead: 112,
                removed: false,
            },
        ],
        dictionaries: vec![DictionaryRef { kind: Kind::Run, pack: 1, offset: 16 }],
        externals: vec![ExternalRef { content: content_of(b"y"), len: 9 }],
        units: vec![content_of(b"crate a 1.0.0"), content_of(b"crate b 2.1.0")],
    };
    let bytes = head.encode();
    assert_eq!(Head::decode(&bytes).unwrap(), head);
    for at in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[at] ^= 1;
        assert!(Head::decode(&changed).is_err(), "byte {at}");
    }
    assert!(Head::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut torn = head.clone();
    torn.packs[0].committed = 4096 + 16;
    assert!(Head::decode(&torn.encode()).is_err(), "a commit ends on a block");
    let mut torn = head.clone();
    torn.packs[1].indexed = 4096 - 1;
    assert!(Head::decode(&torn.encode()).is_err(), "the index ends on a record");
    let mut torn = head.clone();
    torn.packs[1].dead = 100;
    assert!(Head::decode(&torn.encode()).is_err(), "dead bytes are whole records");
}

/// Lengths in a record header are not trusted before they are checked: one whose record would
/// be longer than any number is refused, not wrapped into a record of no bytes.
#[test]
fn a_record_header_of_a_length_no_record_has_is_refused() {
    let header = |stored| RecordHeader {
        kind: Kind::Run,
        form: Form::Raw,
        dictionary: NO_DICTIONARY,
        stored,
        raw: 0,
        key: key(1),
        content: content_of(b""),
    };
    for stored in [0, 1 << 40, u64::MAX - RECORD_HEADER - 15] {
        let decoded = RecordHeader::decode(&header(stored).encode()).unwrap();
        assert_eq!(decoded.span(), (RECORD_HEADER + stored).next_multiple_of(disk::ALIGN));
    }
    for stored in [u64::MAX - RECORD_HEADER - 14, u64::MAX - RECORD_HEADER + 1, u64::MAX] {
        assert!(RecordHeader::decode(&header(stored).encode()).is_err(), "{stored}");
        assert_eq!(disk::span_of(stored), None);
    }
}

#[test]
fn keys_hash_the_schema_the_producer_and_every_input() {
    let schema = SchemaHash(content_of(b"schema"));
    let code = content_of(b"code");
    let (a, b) = (content_of(b"a"), content_of(b"b"));
    let key = Key::of(schema, code, &[a, b]);
    assert_eq!(key, Key::of(schema, code, &[a, b]));
    assert_ne!(key, Key::of(schema, code, &[b, a]));
    assert_ne!(key, Key::of(schema, code, &[a]));
    assert_ne!(key, Key::of(schema, content_of(b"other code"), &[a, b]));
    assert_ne!(key, Key::of(SchemaHash(content_of(b"schema 2")), code, &[a, b]));
}

/// A remote that records what it is asked, and knows without asking it has none of `absent`.
struct Recorded(Vec<(Key, Tag)>, Vec<Key>);

impl Remote for Recorded {
    fn fetch(&mut self, key: &Key, tag: Tag, _: &mut Vec<Effect>) -> Asked {
        if self.1.contains(key) {
            return Asked::Missing;
        }
        self.0.push((*key, tag));
        Asked::Sent
    }
}

#[test]
fn a_fetch_the_remote_knows_is_missing_is_answered_at_once() {
    let mut sim = Sim::new(16, quiet());
    let mut store = open(&mut sim, "store");
    let mut remote = Recorded(Vec::new(), vec![key(5)]);
    let mut effects = Vec::new();
    assert_eq!(store.fetch(&key(5), &mut remote, &mut effects), None);
    assert!(remote.0.is_empty() && effects.is_empty());
    assert_eq!(store.fetched(), [(key(5), Fetched::Missing)]);
    assert!(store.fetch(&key(5), &mut Recorded(Vec::new(), Vec::new()), &mut effects).is_some(), "not left fetching");
}

#[test]
fn keys_lists_every_key_and_new_keys_what_each_batch_put() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"one".to_vec()).unwrap();
    assert_eq!(store.new_keys(), (0, Vec::new()), "nothing is kept before watching");
    store.watch_keys();
    assert!(store.watching_keys());
    store.put(key(2), Kind::Code, b"two".to_vec()).unwrap();
    let mut listed = store.keys();
    listed.sort();
    assert_eq!(listed, {
        let mut both = vec![key(1), key(2)];
        both.sort();
        both
    });
    flush(&mut sim, &mut store);
    let (epoch, mut new) = store.new_keys();
    assert!(epoch > 0, "a batch was moved into packs");
    new.sort();
    new.dedup();
    assert!(new.contains(&key(2)), "{new:?}");
    assert_eq!(store.new_keys(), (epoch, Vec::new()), "each key is taken once");
    store.put(key(3), Kind::Code, b"three".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    let (later, new) = store.new_keys();
    assert!(later > epoch);
    assert!(new.contains(&key(3)) && !new.contains(&key(2)), "{new:?}");
    assert_eq!(store.keys().len(), 3);
}

#[test]
fn a_fetched_value_is_verified_before_it_is_kept() {
    let mut sim = Sim::new(15, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(9), Kind::Code, b"here".to_vec()).unwrap();
    let mut remote = Recorded(Vec::new(), Vec::new());
    let mut effects = Vec::new();
    assert_eq!(store.fetch(&key(9), &mut remote, &mut effects), None, "it has it");
    let good = store.fetch(&key(1), &mut remote, &mut effects).unwrap();
    assert_eq!(store.fetch(&key(1), &mut remote, &mut effects), None, "in flight");
    let bad = store.fetch(&key(2), &mut remote, &mut effects).unwrap();
    let missing = store.fetch(&key(3), &mut remote, &mut effects).unwrap();
    let failed = store.fetch(&key(4), &mut remote, &mut effects).unwrap();
    assert_eq!(remote.0.len(), 4);
    let answer = |answer: remote::Answer| {
        let body = answer.encode();
        Done::Read([(body.len() as u64).to_le_bytes().as_slice(), &body].concat())
    };
    let value = |key, bytes: &[u8], content| remote::Answer::Value {
        key,
        kind: Kind::Code,
        content,
        bytes: bytes.to_vec(),
    };
    store.complete(
        Completion { tag: good, done: answer(value(key(1), b"one", content_of(b"one"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: bad, done: answer(value(key(2), b"evil", content_of(b"two"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: missing, done: answer(remote::Answer::Missing { key: key(3) }) },
        &mut effects,
    );
    store.complete(
        Completion { tag: failed, done: Done::Failed(blaze_core::IoError("no route".into())) },
        &mut effects,
    );
    let fetched: BTreeMap<Key, Fetched> = store.fetched().into_iter().collect();
    assert!(matches!(fetched[&key(1)], Fetched::Kept(_)));
    assert!(matches!(&fetched[&key(2)], Fetched::Refused(why) if why.contains("hash")));
    assert_eq!(fetched[&key(3)], Fetched::Missing);
    assert!(matches!(&fetched[&key(4)], Fetched::Refused(why) if why.contains("no route")));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
    assert!(!store.has(&key(2)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable.len(), 2);
}

#[test]
fn a_store_that_cannot_write_stops_and_says_where() {
    let mut sim = Sim::new(16, Faults { fail_one_in: 1, ..Faults::default() });
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("every effect fails");
    assert_eq!(broken.file, name("store"), "the first thing asked is the check of the directory");
    assert!(broken.what.contains("injected"), "{broken}");
}

#[test]
fn a_driver_may_hand_over_mappings_by_itself() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"mapped".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let mut maps = Vec::new();
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        maps.extend(effects.extract_if(.., |it| matches!(it, Effect::Map { .. })));
    });
    assert!(!store.ready());
    for effect in maps {
        let Effect::Map { file, .. } = effect else { unreachable!() };
        let bytes = sim.contents(&file);
        store.mapped(file, Mapping::copy(&bytes));
    }
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"mapped");
}

/// Every commit leaves each pack a whole number of blocks long, so every append can be written
/// with `O_DIRECT`; the zeros that end a commit are read past at open and never anything else.
#[test]
fn every_commit_ends_each_pack_on_a_block() {
    let mut sim = Sim::new(18, quiet());
    let mut store = open(&mut sim, "store");
    for round in 0..3u64 {
        for n in 0..5 {
            store.put(key(round * 10 + n), Kind::Code, noise(700 + n as usize * 300, n)).unwrap();
        }
        flush(&mut sim, &mut store);
        for pack in &store.tiers[0].packs {
            assert_eq!(pack.committed % disk::BLOCK, 0, "round {round}");
            assert_eq!(sim.contents(&pack.file).len() as u64, pack.committed, "round {round}");
        }
    }
    drop(store);
    let store = open(&mut sim, "store");
    for round in 0..3u64 {
        for n in 0..5 {
            let found = bytes_of(&store, &key(round * 10 + n)).unwrap();
            assert_eq!(found, noise(700 + n as usize * 300, n));
        }
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_changed_byte_in_the_zeros_ending_a_commit_stops_the_store_naming_its_pack() {
    let mut sim = Sim::new(19, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, vec![7; 100]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    let last = bytes.len() - 1;
    bytes[last] = 1;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the zeros are checked");
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("zeros"), "{broken}");
}

#[test]
fn an_index_that_does_not_match_its_crc_stops_the_store() {
    let mut sim = Sim::new(20, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(n), Kind::Query, n.to_le_bytes().to_vec()).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.tiers[0].head.index.unwrap().slot, 0);
    drop(store);
    let index = name("store/index-0");
    let mut bytes = sim.contents(&index);
    let at = bytes.len() / 2;
    bytes[at] ^= 0x10;
    sim.change(&index, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the index is checked at open");
    assert_eq!(broken.file, index);
    assert!(broken.what.contains("CRC-32C"), "{broken}");
}

#[test]
fn a_long_cold_value_is_compressed_without_a_dictionary_over_a_long_window() {
    let mut sim = Sim::new(21, quiet());
    let mut store = open(&mut sim, "store");
    let mut long = Vec::new();
    let mut n = 0;
    while long.len() < cold::LONG_FROM + 1000 {
        long.extend(run_output(n));
        n += 1;
    }
    store.put(key(1), Kind::Debug, long.clone()).unwrap();
    let (form, stored, raw) = store.sizes_of(&key(1));
    assert_eq!(form, Form::Zstd);
    assert!(stored * 4 < raw, "{stored} stored of {raw}");
    assert!(store.tiers[0].dictionaries.is_empty(), "a long value is not a sample");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), long);
    flush(&mut sim, &mut store);
    let loc = store.loc_of(&key(1));
    let header = RecordHeader::decode(store.tiers[0].packs[loc.pack as usize].bytes(loc.offset, RECORD_HEADER).unwrap());
    assert_eq!(header.unwrap().dictionary, NO_DICTIONARY);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), long);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn chunks_that_shrink_are_kept_in_lz4_and_the_rest_raw() {
    let mut sim = Sim::new(22, quiet());
    let mut store = open(&mut sim, "store");
    let mut text = Vec::new();
    let mut n = 0;
    while text.len() < 600 << 10 {
        text.extend(run_output(n));
        n += 1;
    }
    let mut both = text.clone();
    both.extend(noise(600 << 10, 220));
    store.put(key(1), Kind::Output, both.clone()).unwrap();
    let forms: Vec<Form> = store.tiers[0]
        .delta
        .iter(Held::writer(&store))
        .filter(|(_, it)| it.key != key(1))
        .map(|(_, it)| it.place.form())
        .collect();
    assert!(forms.contains(&Form::Lz4), "{forms:?}");
    assert!(forms.contains(&Form::Raw), "{forms:?}");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), both);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), both);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn what_is_unread_for_two_periods_goes_cold_and_what_is_read_stays_hot() {
    let mut sim = Sim::new(30, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..10 {
        store.put(key(n), Kind::Code, text(n, 8 << 10)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "one period is not two");
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a record made in the last two periods stays");
    assert_eq!(bytes_of(&store, &key(0)).unwrap(), text(0, 8 << 10));
    let settled = settle(&mut sim, &mut store);
    assert_eq!(settled.demoted, 9, "{settled:?}");
    assert_eq!(form_of(&store, &key(0)), Form::Raw, "it was read");
    for n in 1..10 {
        assert_eq!(form_of(&store, &key(n)), Form::Zstd, "{n}");
    }
    let durable = store.put(key(3), Kind::Code, text(3, 8 << 10)).unwrap();
    assert!(store.durable(durable), "a moved value is as durable as it was");
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..10 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), text(n, 8 << 10), "{n}");
    }
    assert_eq!(form_of(&store, &key(4)), Form::Zstd);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_cold_value_read_in_two_periods_running_comes_back_hot() {
    let mut sim = Sim::new(31, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, text(1, 16 << 10)).unwrap();
    store.put(key(2), Kind::Output, text(2, 16 << 10)).unwrap();
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 2);
    for round in 0..2 {
        assert_eq!(bytes_of(&store, &key(1)).unwrap(), text(1, 16 << 10));
        assert_eq!(bytes_of(&store, &key(2)).unwrap(), text(2, 16 << 10));
        let settled = settle(&mut sim, &mut store);
        assert_eq!(settled.promoted, round * 2, "{settled:?}");
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert_eq!(form_of(&store, &key(2)), Form::Lz4, "an output comes back as LZ4");
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), text(2, 16 << 10));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_value_that_does_not_compress_is_tried_once() {
    let mut sim = Sim::new(32, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, noise(8 << 10, 320)).unwrap();
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    assert!(store.flag_of(&key(1), Flag::Tried));
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert_eq!(settle(&mut sim, &mut store), Settled::default());
}

#[test]
fn packs_that_are_mostly_moved_out_are_rewritten_and_removed() {
    let mut sim = Sim::new(33, quiet());
    let mut store = open(&mut sim, "store");
    let value = |n: u64| if n % 2 == 0 { text(n, 12 << 10) } else { noise(4 << 10, n) };
    for n in 0..100 {
        store.put(key(n), Kind::Code, value(n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let before: Vec<FileName> = store.tiers[0].packs.iter().map(|it| it.file.clone()).collect();
    assert!(before.len() >= 3, "{} packs", before.len());
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let settled = settle(&mut sim, &mut store);
    assert_eq!(settled.demoted, 50, "{settled:?}");
    assert!(settled.retired >= 2, "{settled:?}");
    let last = before.len() - 1;
    for (at, file) in before.iter().enumerate() {
        let removed = store.tiers[0].packs[at].removed;
        assert_eq!(removed, !sim.exists(file), "{file}");
        if at == last {
            assert!(!removed, "the pack its kind appends to stays");
        }
    }
    assert!(store.tiers[0].packs.iter().filter(|it| it.removed).count() as u64 == settled.retired);
    for n in 0..100 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), value(n), "{n}");
    }
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..100 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), value(n), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_pack_left_behind_by_a_crash_is_removed_at_the_next_open() {
    let mut sim = Sim::new(34, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..60 {
        store.put(key(n), Kind::Code, text(n, 12 << 10)).unwrap();
    }
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let mut effects = Vec::new();
    assert!(store.settle(&mut effects).retired >= 1);
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        effects.retain(|it| !matches!(it, Effect::Remove { .. }));
    });
    let gone: Vec<FileName> =
        store.tiers[0].packs.iter().filter(|it| it.removed).map(|it| it.file.clone()).collect();
    assert!(!gone.is_empty() && gone.iter().all(|it| sim.exists(it)), "the removes were dropped");
    drop(store);
    let store = open(&mut sim, "store");
    assert!(gone.iter().all(|it| !sim.exists(it)));
    for n in 0..60 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), text(n, 12 << 10), "{n}");
    }
}

/// A schema of one `u32` column, encoded by blaze-format's tiers: what a query's result made
/// of columns is to the store. `broken` encodings give back other values from warm.
pub(super) struct OneColumn {
    schema: SchemaHash,
    warm_schema: SchemaHash,
    broken: bool,
}

const ONE_COLUMN_HEAD: usize = 8 + 32 + 8;
const WARM_HEAD: usize = 8 + 32 + 4 + 4 + 8;

fn one_column(broken: bool) -> OneColumn {
    OneColumn {
        schema: SchemaHash(ContentId::of("blaze v2 store test schema", b"one column")),
        warm_schema: SchemaHash(ContentId::of("blaze v2 store test schema", b"one column, warm")),
        broken,
    }
}

fn word<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    bytes[at..at + N].try_into().unwrap()
}

impl OneColumn {
    fn hot(&self, values: &[u32]) -> Vec<u8> {
        let mut out = blaze_format::MAGIC.to_vec();
        out.extend(self.schema.0.0);
        out.extend((values.len() as u64).to_le_bytes());
        out.extend(values.iter().flat_map(|it| it.to_le_bytes()));
        out
    }

    fn values(&self, hot: &[u8]) -> Result<Vec<u32>, String> {
        if hot.len() < ONE_COLUMN_HEAD || hot[8..40] != self.schema.0.0 {
            return Err("not a value of the schema".into());
        }
        let len = u64::from_le_bytes(word(hot, 40)) as usize;
        let rest = &hot[ONE_COLUMN_HEAD..];
        if rest.len() != len * 4 {
            return Err("not as long as it says".into());
        }
        Ok(rest.chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect())
    }
}

impl OneColumn {
    pub(super) fn schema(&self) -> SchemaHash {
        self.schema
    }

    pub(super) fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        let packed = blaze_format::tiers::Packed::pack(&self.values(hot)?);
        let mut out = blaze_format::MAGIC.to_vec();
        out.extend(self.warm_schema.0.0);
        out.extend(packed.min.to_le_bytes());
        out.extend(packed.bits.to_le_bytes());
        out.extend((packed.len as u64).to_le_bytes());
        out.extend(packed.words.iter().flat_map(|it| it.to_le_bytes()));
        Ok(out)
    }

    pub(super) fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, String> {
        if warm.len() < WARM_HEAD || warm[8..40] != self.warm_schema.0.0 {
            return Err("not warm columns of the schema".into());
        }
        let words = warm[WARM_HEAD..].chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap()));
        let packed = blaze_format::tiers::Packed {
            min: u32::from_le_bytes(word(warm, 40)),
            bits: u32::from_le_bytes(word(warm, 44)),
            len: u64::from_le_bytes(word(warm, 48)) as usize,
            words: words.collect(),
        };
        let mut values = packed.unpack();
        if self.broken {
            values[0] += 1;
        }
        Ok(self.hot(&values))
    }

    pub(super) fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        Ok(blaze_format::tiers::encode_cold(&self.values(hot)?, None))
    }

    pub(super) fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, String> {
        let values = blaze_format::tiers::decode_cold(cold, None).map_err(|it| it.to_string())?;
        Ok(self.hot(&values))
    }
}

/// A column of 4096 values in a range of 50: 16 KiB hot, 6 bits a value warm.
fn column_values(n: u32) -> Vec<u32> {
    (0..4096).map(|at| 1000 + n + at * 7 % 50).collect()
}

fn with_columns(sim: &mut Sim, broken: bool) -> Store {
    let mut store = open(sim, "store");
    store.add_columns(Columns::Test(one_column(broken)));
    store
}

#[test]
fn a_value_of_columns_goes_warm_then_cold_then_comes_back_hot() {
    let mut sim = Sim::new(40, quiet());
    let mut store = with_columns(&mut sim, false);
    let columns = one_column(false);
    let hot = columns.hot(&column_values(0));
    store.put(key(1), Kind::Query, hot.clone()).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..2 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    assert!(store.sizes_of(&key(1)).1 < hot.len() as u64 / 4);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a warm record made last period stays");
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    for round in 0..2 {
        assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
        let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
        assert_eq!((heat, entry.bytes, entry.content), (Heat::Hot, hot.as_slice(), content_of(&hot)));
        assert_eq!(settle(&mut sim, &mut store).promoted, round);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn warm_columns_are_read_in_place_and_refused_without_their_encoding() {
    let mut sim = Sim::new(41, quiet());
    let mut store = with_columns(&mut sim, false);
    let columns = one_column(false);
    let hot = columns.hot(&column_values(3));
    store.put(key(1), Kind::Query, hot.clone()).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    let warm = columns.warm(&hot).unwrap();
    let (heat, first) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!((heat, first.bytes, first.content), (Heat::Warm, warm.as_slice(), content_of(&hot)));
    let (_, again) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!(first.bytes.as_ptr(), again.bytes.as_ptr(), "read where it is kept");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot, "a get gives the value as put");
    drop(store);
    let store = open(&mut sim, "store");
    let refused = store.get(&key(1)).unwrap_err();
    assert!(refused.what.contains("no encoding"), "{refused}");
    assert!(store.get_in_place(&key(1)).is_err());
    assert_eq!(store.verify(None).len(), 1);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(store.get_in_place(&key(1)).unwrap().unwrap().1.bytes, warm.as_slice());
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn columns_that_do_not_decode_to_the_value_are_not_kept() {
    let mut sim = Sim::new(42, quiet());
    let mut store = with_columns(&mut sim, true);
    let hot = one_column(true).hot(&column_values(5));
    store.put(key(1), Kind::Query, hot.clone()).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert!(store.problems().iter().any(|it| it.contains("do not decode")), "{:?}", store.problems());
    assert!(store.flag_of(&key(1), Flag::Tried));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
}

#[test]
fn a_cold_kind_of_columns_is_put_in_cold_columns() {
    let mut sim = Sim::new(43, quiet());
    let mut store = with_columns(&mut sim, false);
    let hot = one_column(false).hot(&column_values(9));
    store.put(key(1), Kind::Run, hot.clone()).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    flush(&mut sim, &mut store);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

/// A span table whose starts climb by 12 and whose lengths cycle through eight: a `blaze_format`
/// buffer of the kind every schema given to [`Store::encode_columns`] is.
fn spans_buffer() -> Vec<u8> {
    let mut spans = blaze_format::Spans::new(ContentId::of("blaze v2 store test", b"spans"));
    for at in 0..4096 {
        _ = spans.push(at * 12, 4 + at % 8);
    }
    blaze_format::to_bytes(&spans)
}

fn with_spans(sim: &mut Sim) -> Store {
    let mut store = open(sim, "store");
    store.encode_columns(blaze_format::Spans::columns());
    store
}

#[test]
fn a_buffer_given_its_buffer_columns_goes_warm_then_cold_and_reads_back() {
    let mut sim = Sim::new(44, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Query, hot.clone()).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..2 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    let warm = blaze_format::Spans::columns().warm(&hot).unwrap();
    let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!((heat, entry.bytes, entry.content), (Heat::Warm, warm.as_slice(), content_of(&hot)));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a warm record made last period stays");
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "nor one read the period before");
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert!(store.get(&key(1)).unwrap_err().what.contains("no encoding"));
    drop(store);
    let store = with_spans(&mut sim);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_cold_kind_of_buffer_is_put_in_its_cold_columns() {
    let mut sim = Sim::new(45, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Run, hot.clone()).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
}

#[test]
fn each_crate_version_has_packs_of_its_own_and_keeps_them_across_an_open() {
    let mut sim = Sim::new(50, quiet());
    let mut store = open(&mut sim, "store");
    let (a, b) = (content_of(b"crate a 1.0.0"), content_of(b"crate b 2.1.0"));
    let unit_a = store.unit(a);
    let unit_b = store.unit(b);
    assert_eq!(store.unit(a), unit_a, "a crate version is numbered once");
    assert_ne!(unit_a, unit_b);
    for n in 0..6 {
        let unit = if n % 2 == 0 { unit_a } else { unit_b };
        store.put_in(unit, key(n), [Kind::Query, Kind::Code, Kind::Debug][n as usize % 3], noise(5000, n)).unwrap();
    }
    store.put(key(10), Kind::Image, noise(3000, 10)).unwrap();
    flush(&mut sim, &mut store);
    let pack_of = |store: &Store, n: u64| store.loc_of(&key(n)).pack as usize;
    let units: Vec<u32> = (0..6).map(|n| store.tiers[0].packs[pack_of(&store, n)].unit).collect();
    assert_eq!(units, [unit_a.0, unit_b.0, unit_a.0, unit_b.0, unit_a.0, unit_b.0]);
    assert_eq!(pack_of(&store, 0), pack_of(&store, 2), "kinds of one crate version share its file");
    assert_eq!(store.tiers[0].packs[pack_of(&store, 10)].unit, Unit::SHARED.0);
    assert_eq!(store.tiers[0].packs.len(), 3);
    drop(store);
    let mut store = open(&mut sim, "store");
    assert_eq!(store.unit(b), unit_b, "the head keeps the numbering");
    assert_eq!(store.tiers[0].units, [a, b]);
    for n in 0..6 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), noise(5000, n), "{n}");
    }
    store.put_in(unit_a, key(20), Kind::Query, noise(100, 20)).unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(pack_of(&store, 20), pack_of(&store, 0), "a unit appends to its own pack");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_pack_whose_header_names_another_unit_stops_the_store() {
    let mut sim = Sim::new(51, quiet());
    let mut store = open(&mut sim, "store");
    let unit = store.unit(content_of(b"crate a 1.0.0"));
    store.put_in(unit, key(1), Kind::Query, noise(100, 1)).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[8] = 0;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the unit is checked");
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("unit"), "{broken}");
}

/// `column` repeated to at least 8 KiB, so the tier mover (which leaves records under
/// `DEMOTE_FROM` where they are) moves every shape but the empty one.
fn movable_column(column: &[u32]) -> Vec<u32> {
    let times = match column.len() {
        0 => 1,
        len => (8usize << 10).div_ceil(4 * len),
    };
    column.repeat(times)
}

/// The record's form and values, read without counting as a read, so looking does not change
/// where the mover sends it.
fn peek(store: &Store, key: &Key) -> (Form, Vec<u32>) {
    let found = store.found_of(key);
    let (_, bytes) = store.look().value(found, key, Use::Check).unwrap();
    (found.place.form(), one_column(false).values(&bytes).unwrap())
}

/// What a column read as at each step of its record's life in the store.
struct Trip {
    len: usize,
    stages: Vec<(&'static str, Form, Vec<u32>)>,
}

/// Puts `column` as a value of columns, lets the mover take it as cold as it goes (two
/// unread periods a tier), reads it in two periods running so it comes back hot, and reopens.
fn through_the_tiers(column: &[u32], forms: &std::cell::RefCell<Vec<Form>>) -> Trip {
    let mut sim = Sim::new(60, quiet());
    let mut store = with_columns(&mut sim, false);
    let hot = one_column(false).hot(&movable_column(column));
    store.put(key(1), Kind::Query, hot).unwrap();
    flush(&mut sim, &mut store);
    let mut trip = Trip { len: column.len(), stages: Vec::new() };
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("put", form, values));
    for (stage, settles) in [("first move", 3), ("second move", 2)] {
        for _ in 0..settles {
            settle(&mut sim, &mut store);
        }
        let (form, values) = peek(&store, &key(1));
        forms.borrow_mut().push(form);
        trip.stages.push((stage, form, values));
    }
    for _ in 0..2 {
        store.get(&key(1)).unwrap().unwrap();
        settle(&mut sim, &mut store);
    }
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("read back hot", form, values));
    drop(store);
    let store = with_columns(&mut sim, false);
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("reopened", form, values));
    assert_eq!(store.verify(None), Vec::new());
    trip
}

/// The column every stage of `trip` read, when they all read the same and it came back raw.
fn column_of(trip: &Trip) -> Result<Vec<u32>, String> {
    let (_, _, first) = &trip.stages[0];
    for (stage, form, values) in &trip.stages {
        if values != first {
            return Err(format!("{stage} ({form:?}) read other values than were put"));
        }
    }
    let (_, last, _) = trip.stages.last().unwrap();
    if trip.len > 0 && *last != Form::Raw {
        return Err(format!("read in two periods running, it stayed {last:?}"));
    }
    Ok(first[..trip.len].to_vec())
}

#[test]
fn a_record_moved_hot_to_warm_to_cold_and_back_round_trips_every_column_shape() {
    for seed in [1, 2, 3] {
        let forms = std::cell::RefCell::new(Vec::new());
        let checked =
            blaze_v2_proof::columns::round_trips(|column| through_the_tiers(column, &forms), column_of, seed)
                .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(checked > 100, "{checked} shapes");
        let forms = forms.into_inner();
        assert!(forms.contains(&Form::Warm), "no shape went warm: {forms:?}");
        assert!(forms.contains(&Form::ColdColumns), "no shape went cold: {forms:?}");
    }
}

/// A column as the store leaves it after the mover's first move: read in place where it is.
enum InPlace {
    Hot(Vec<u8>),
    Warm(blaze_format::tiers::Packed),
}

fn moved_once(column: &[u32]) -> InPlace {
    let mut sim = Sim::new(61, quiet());
    let mut store = with_columns(&mut sim, false);
    store.put(key(1), Kind::Query, one_column(false).hot(&movable_column(column))).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        settle(&mut sim, &mut store);
    }
    let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
    match heat {
        Heat::Hot => InPlace::Hot(entry.bytes.to_vec()),
        Heat::Warm => {
            let warm = entry.bytes;
            let words = warm[WARM_HEAD..].chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap()));
            InPlace::Warm(blaze_format::tiers::Packed {
                min: u32::from_le_bytes(word(warm, 40)),
                bits: u32::from_le_bytes(word(warm, 44)),
                len: u64::from_le_bytes(word(warm, 48)) as usize,
                words: words.collect(),
            })
        }
    }
}

fn element(column: &InPlace, at: usize) -> u32 {
    match column {
        InPlace::Hot(bytes) => u32::from_le_bytes(word(bytes, ONE_COLUMN_HEAD + 4 * at)),
        InPlace::Warm(packed) => packed.as_ref().get(at),
    }
}

#[test]
fn what_the_mover_leaves_warm_or_hot_reads_in_place_for_every_column_shape() {
    for seed in [1, 2, 3] {
        let checked = blaze_v2_proof::columns::reads_in_place(moved_once, element, seed)
            .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(checked > 100, "{checked} shapes");
    }
}

/// Calls `tidy` until the store's limbo and its delta tables hold nothing swapped out and no
/// index swap is waiting on writers. Other tests of this process hold guards of their own, so a
/// grace can take a moment to pass.
fn reclaim_all(store: &mut Store) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let retired = |store: &Store| {
        let own = &store.tiers[0];
        std::iter::once(&own.delta).chain(&own.prev).map(|it| it.retired()).sum::<usize>()
    };
    while !store.limbo.is_empty() || store.migrating.is_some() || retired(store) > 0 {
        store.tidy();
        assert!(std::time::Instant::now() < deadline, "a grace never passed with no task of this test open");
        std::thread::yield_now();
    }
}

/// A worker's task reads each value it puts and holds what it read, while the store takes one
/// batch of completions at a time through commits that move those values out of their
/// segments, swap the index and retire views, spots and chunks. What the task read stays whole
/// until it ends; then all of it is freed, every put is durable, and the store reopens with
/// every value.
#[test]
fn what_a_task_read_stays_whole_through_every_step_of_commits_and_is_freed_after() {
    for seed in 0..16 {
        let mut sim = Sim::new(200 + seed, quiet());
        let mut store = open(&mut sim, "store");
        let mut writer = store.writer();
        let reader = store.reader();
        let guard = enter();
        let (mut values, mut read, mut tickets) = (Vec::new(), Vec::new(), Vec::new());
        let (mut effects, mut out) = (Vec::new(), Vec::new());
        for n in 0..INDEX_AFTER as u64 * 3 {
            let (kind, bytes) = match n % 9 {
                // Outputs alike but for their ends: their chunks are put once.
                0 => (Kind::Output, noise(CHUNK_FROM + 1000 + n as usize, seed)),
                1 => (Kind::Query, noise(EXTERNAL_FROM + n as usize, n)),
                _ => (Kind::Query, noise(40 + (n as usize * 53) % 3000, n)),
            };
            tickets.push(writer.put(&guard, key(n), kind, bytes.clone()).unwrap());
            let entry = reader.get(&guard, &key(n)).unwrap().expect("a put is read at once");
            assert_eq!(entry.bytes, bytes.as_slice(), "seed {seed}");
            read.push(entry.bytes);
            values.push(bytes);
            match sim.rng().below(3) {
                0 => store.flush(&mut effects),
                _ => {
                    sim.submit(&mut effects);
                    sim.poll(&mut out, Duration::ZERO);
                    for completion in out.drain(..) {
                        if store.owns(completion.tag) {
                            store.complete(completion, &mut effects);
                        }
                    }
                }
            }
            if n % 16 == 15 {
                for (at, bytes) in read.iter().enumerate() {
                    assert_eq!(*bytes, values[at].as_slice(), "seed {seed}: value {at} changed under its task");
                }
            }
        }
        drive(&mut sim, &mut store, &mut effects);
        flush(&mut sim, &mut store);
        for (at, bytes) in read.iter().enumerate() {
            assert_eq!(*bytes, values[at].as_slice(), "seed {seed}: value {at} changed under its task");
        }
        assert!(store.tiers[0].head.index.is_some(), "seed {seed}: an index was written");
        assert!(!store.limbo.is_empty(), "seed {seed}: what the task can still read is held");
        drop(read);
        drop(guard);
        reclaim_all(&mut store);
        assert!(store.tiers[0].prev.is_none(), "seed {seed}: the swap ended");
        flush(&mut sim, &mut store);
        assert!(tickets.iter().all(|it| store.durable(*it)), "seed {seed}: a put is not durable");
        drop(writer);
        drop(store);
        let store = open(&mut sim, "store");
        for (n, bytes) in values.iter().enumerate() {
            assert_eq!(bytes_of(&store, &key(n as u64)).as_ref(), Some(bytes), "seed {seed}: {n}");
        }
        assert_eq!(store.verify(None), Vec::new(), "seed {seed}");
    }
}

/// Four writers on threads of their own put every key at once, each from another place in
/// the list, so they race to claim keys and the chunks their outputs share; two readers read
/// every key the while; and the store's thread commits, settles and swaps its index under
/// them. Every value read is whole and what was put, every put is durable after the last
/// flush, and the store reopens with every value.
#[test]
fn writers_and_readers_on_other_threads_go_on_through_commits_settles_and_index_swaps() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const WRITERS: u64 = 4;
    const KEYS: u64 = 240;
    let values: Arc<Vec<(Kind, Vec<u8>)>> = Arc::new(
        (0..KEYS)
            .map(|n| match n % 20 {
                0 => {
                    let mut bytes = noise(CHUNK_FROM + (200 << 10), 7);
                    let at = (n as usize * 4099) % bytes.len();
                    bytes[at] ^= 0x5a;
                    (Kind::Output, bytes)
                }
                1 => (Kind::Query, noise(EXTERNAL_FROM + n as usize, n)),
                2 | 3 => (Kind::Run, run_output(n)),
                _ => (Kind::Query, noise(32 + (n as usize * 37) % 2000, n)),
            })
            .collect(),
    );
    let mut sim = Sim::new(300, quiet());
    let mut store = open(&mut sim, "store");
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..2u64)
        .map(|first| {
            let (reader, values, stop) = (store.reader(), values.clone(), stop.clone());
            std::thread::spawn(move || {
                let (mut found, mut at) = (0u64, first);
                while !stop.load(Ordering::Relaxed) {
                    let guard = enter();
                    for _ in 0..16 {
                        at = (at + 31) % KEYS;
                        let (kind, bytes) = &values[at as usize];
                        let entry = reader.get(&guard, &key(at)).unwrap_or_else(|corrupt| panic!("{corrupt}"));
                        if let Some(entry) = entry {
                            assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()), "key {at}");
                            found += 1;
                        }
                    }
                    drop(guard);
                }
                found
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|number| {
            let (mut writer, values) = (store.writer(), values.clone());
            std::thread::spawn(move || {
                let mut tickets = Vec::new();
                for i in 0..KEYS {
                    let n = (i + number * KEYS / WRITERS) % KEYS;
                    let (kind, bytes) = &values[n as usize];
                    let guard = enter();
                    tickets.push(writer.put(&guard, key(n), *kind, bytes.clone()).unwrap());
                }
                tickets
            })
        })
        .collect();
    let mut rounds = 0u32;
    while !writers.iter().all(|it| it.is_finished()) {
        flush(&mut sim, &mut store);
        if rounds % 4 == 3 {
            settle(&mut sim, &mut store);
        }
        rounds += 1;
        std::thread::yield_now();
    }
    let tickets: Vec<Ticket> = writers.into_iter().flat_map(|it| it.join().unwrap()).collect();
    flush(&mut sim, &mut store);
    assert!(store.broken().is_none(), "{:?}", store.broken());
    assert_eq!(tickets.len() as u64, WRITERS * KEYS);
    assert!(tickets.iter().all(|it| store.durable(*it)), "every put is durable after the last flush");
    stop.store(true, Ordering::Relaxed);
    let found: u64 = readers.into_iter().map(|it| it.join().unwrap()).sum();
    assert!(found > 0, "the readers read");
    for (n, (kind, bytes)) in values.iter().enumerate() {
        let entry = store.get(&key(n as u64)).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()), "{n}");
    }
    assert!(store.tiers[0].head.index.is_some(), "the index was swapped under them");
    drop(store);
    let store = open(&mut sim, "store");
    for (n, (_, bytes)) in values.iter().enumerate() {
        assert_eq!(bytes_of(&store, &key(n as u64)).as_ref(), Some(bytes), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

/// A lookup that hits allocates nothing wherever its record is (a writer's segment, a pack a
/// delta spot points at, the mapped index), the first read of a record (which verifies and
/// flags it) included; nor does a warm writer's put, past chunk rolls and past the delta
/// table's high-water mark into a new level.
#[test]
fn hits_and_warm_puts_allocate_nothing() {
    let mut sim = Sim::new(31, quiet());
    let mut store = open(&mut sim, "store");
    let mut writer = store.writer();
    let reader = store.reader();
    let count = INDEX_AFTER as u64 * 3;
    let values: Vec<Vec<u8>> = (0..count).map(|n| noise(40 + (n as usize * 53) % 3000, n)).collect();
    let guard = enter();
    let hits = |store: &Store, upto: u64, what: &str| {
        blaze_alloc_count::assert_at_most(0, what, || {
            for n in 0..upto {
                let want = values[n as usize].as_slice();
                let entry = reader.get(&guard, &key(n)).unwrap().expect("put before");
                assert!(entry.bytes == want, "{what}: {n}");
                assert!(store.get(&key(n)).unwrap().is_some_and(|it| it.bytes == want), "{what}: {n}");
            }
        });
    };
    // The first put warms what a thread keeps once: its writer's first chunk is made already.
    writer.put(&guard, key(0), Kind::Query, values[0].clone()).unwrap();
    let mut pending: Vec<(Key, Vec<u8>)> = (1..count - 16).map(|n| (key(n), values[n as usize].clone())).collect();
    let before = store.tiers[0].delta.len();
    blaze_alloc_count::assert_at_most(0, "warm puts", || {
        for (key, bytes) in pending.drain(..) {
            writer.put(&guard, key, Kind::Query, bytes).unwrap();
        }
    });
    assert!(store.tiers[0].delta.len() > before + DELTA_FIRST / 2, "the puts went past the delta's first level");
    hits(&store, count - 16, "hits in segments");
    drop(guard);
    flush(&mut sim, &mut store);
    reclaim_all(&mut store);
    let guard = enter();
    let mut rest: Vec<(Key, Vec<u8>)> = (count - 16..count).map(|n| (key(n), values[n as usize].clone())).collect();
    blaze_alloc_count::assert_at_most(0, "warm puts after a commit", || {
        for (key, bytes) in rest.drain(..) {
            writer.put(&guard, key, Kind::Query, bytes).unwrap();
        }
    });
    drop(guard);
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].head.index.is_some(), "an index was written");
    let guard = enter();
    let hits = |store: &Store, what: &str| {
        blaze_alloc_count::assert_at_most(0, what, || {
            for n in 0..count {
                let entry = reader.get(&guard, &key(n)).unwrap().expect("put before");
                assert!(entry.bytes == values[n as usize].as_slice(), "{what}: {n}");
                let in_place = reader.get_in_place(&guard, &key(n)).unwrap();
                assert!(in_place.is_some_and(|(_, it)| it.bytes == values[n as usize].as_slice()), "{what}: {n}");
                assert!(store.has(&key(n)), "{what}: {n}");
            }
        });
    };
    hits(&store, "hits in packs and the index");
    hits(&store, "hits again");
}

/// A pinned value is the value put, at a 16-byte boundary, held past the task and the store:
/// in place (the store's own bytes) for a raw record of a mapped pack at a 16-byte boundary,
/// a copy for one in a writer's segment or kept compressed. A pinned hit in place allocates
/// nothing but the count of its mapping.
#[test]
fn a_pinned_value_is_held_in_place_past_its_task_and_its_store() {
    let mut sim = Sim::new(33, quiet());
    let mut store = open(&mut sim, "store");
    let values: Vec<Vec<u8>> = (0..8).map(|n| text(n, (8 << 10) + n as usize * 7)).collect();
    for (n, value) in values.iter().enumerate() {
        store.put(key(n as u64), Kind::Code, value.clone()).unwrap();
    }
    let aligned = |bytes: &[u8]| (bytes.as_ptr() as usize).is_multiple_of(16);
    let in_segment = store.pinned(&key(0)).unwrap().unwrap();
    assert!(!in_segment.in_place(), "a record in a writer's segment is copied");
    assert_eq!(&*in_segment, values[0].as_slice());
    assert!(aligned(&in_segment));
    flush(&mut sim, &mut store);
    let mut pinned = Vec::new();
    for (n, value) in values.iter().enumerate() {
        let it = store.pinned(&key(n as u64)).unwrap().unwrap();
        assert_eq!((&*it, it.kind, it.content), (value.as_slice(), Kind::Code, content_of(value)), "{n}");
        assert!(aligned(&it), "{n}");
        let kept = store.get(&key(n as u64)).unwrap().unwrap().bytes;
        assert_eq!(it.in_place(), aligned(kept), "{n}: in place exactly when the record is aligned");
        if it.in_place() {
            assert_eq!(it.as_ptr(), kept.as_ptr(), "{n}: the store's own bytes");
        }
        pinned.push(it);
    }
    assert!(store.pinned(&key(99)).unwrap().is_none());
    // Unread for two periods, all but key 0 go to zstd; a pinned read of one is a copy.
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(bytes_of(&store, &key(0)).unwrap(), values[0]);
    settle(&mut sim, &mut store);
    assert_eq!(form_of(&store, &key(1)), Form::Zstd);
    let cold = store.pinned(&key(1)).unwrap().unwrap();
    assert!(!cold.in_place());
    assert_eq!(&*cold, values[1].as_slice());
    let reader = store.reader();
    let guard = enter();
    let read = reader.pinned(&guard, &key(0)).unwrap().unwrap();
    drop(guard);
    reclaim_all(&mut store);
    drop(store);
    for (n, it) in pinned.iter().enumerate() {
        assert_eq!(&**it, values[n].as_slice(), "{n}: held past the settle and the store");
    }
    assert_eq!(&*read, values[0].as_slice());
}

/// A module of machine code is a file of its own however short, never moved colder, and once
/// committed its pin names that file, whose bytes are exactly the value's; a record of a pack
/// names none.
#[test]
fn a_module_of_machine_code_is_a_file_of_its_own_that_its_pin_names() {
    let mut sim = Sim::new(34, quiet());
    let mut store = open(&mut sim, "store");
    let code = noise(5000, 34);
    store.put(key(1), Kind::Machine, code.clone()).unwrap();
    store.put(key(2), Kind::Code, text(2, 5000)).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::External);
    let in_segment = store.pinned(&key(1)).unwrap().unwrap();
    assert_eq!(in_segment.file(), None, "no file until its commit writes it");
    assert_eq!(&*in_segment, code.as_slice());
    flush(&mut sim, &mut store);
    let file = name(&format!("store/{}", disk::external_file(&content_of(&code))));
    assert_eq!(sim.contents(&file), code);
    let pinned = store.pinned(&key(1)).unwrap().unwrap();
    assert!(pinned.in_place());
    assert_eq!(pinned.file(), Some(&file));
    assert_eq!(&*pinned, code.as_slice());
    assert_eq!(store.pinned(&key(2)).unwrap().unwrap().file(), None, "a record of a pack is no file of its own");
    for _ in 0..4 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(form_of(&store, &key(1)), Form::External, "machine code is never compressed");
    drop((pinned, in_segment, store));
    let store = open(&mut sim, "store");
    let reopened = store.pinned(&key(1)).unwrap().unwrap();
    assert_eq!((reopened.file(), &*reopened), (Some(&file), code.as_slice()));
}

/// A handle reads beside the store and puts through a writer of its own: what it puts is read
/// at once, by it and by the store, refused with other bytes under the same key, durable with
/// the store's next flush and there after reopening. A hit through it allocates nothing.
#[test]
fn a_handle_reads_and_puts_beside_the_store_and_its_puts_are_durable_with_a_flush() {
    let mut sim = Sim::new(35, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, b"the store's".to_vec()).unwrap();
    let root = std::path::PathBuf::from("/state");
    let handle = Handle::new(&mut store, root.clone());
    assert_eq!(handle.read(&key(1), |it| it.bytes.to_vec()).unwrap().unwrap(), b"the store's");
    assert!(handle.has(&key(1)) && !handle.has(&key(2)));
    assert!(handle.read(&key(2), |_| ()).unwrap().is_none());
    let code = noise(3000, 35);
    let ticket = handle.put(key(2), Kind::Machine, code.clone()).unwrap();
    assert!(!handle.durable(ticket));
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), code);
    assert_eq!(&*handle.pinned(&key(2)).unwrap().unwrap(), code.as_slice());
    assert_eq!(handle.put(key(2), Kind::Machine, code.clone()), Ok(ticket), "the same bytes again are the first put");
    assert!(handle.put(key(2), Kind::Machine, b"other".to_vec()).is_err());
    flush(&mut sim, &mut store);
    assert!(handle.durable(ticket));
    let pinned = handle.pinned(&key(2)).unwrap().unwrap();
    let file = pinned.file().expect("a module of machine code is a file of its own");
    assert_eq!(handle.path_of(file), root.join(file.as_str()));
    assert_eq!(sim.contents(file), code);
    _ = handle.read(&key(1), |it| it.bytes.len());
    blaze_alloc_count::assert_at_most(0, "hits through a handle", || {
        assert_eq!(handle.read(&key(1), |it| it.bytes.len()).unwrap(), Some(11));
        assert!(handle.has(&key(2)));
    });
    drop((pinned, handle, store));
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), code);
}

/// On this machine's disk, under the native driver: a module of machine code put through a
/// handle is, after a flush, a file at the handle's path of its pin holding exactly its bytes,
/// and there in a store opened again.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_driven_store_keeps_machine_code_as_a_file_on_disk() {
    let root = std::env::temp_dir().join(format!("blaze-cas-driven-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    let code = noise(70_000, 37);
    {
        let mut driven = Driven::open(&root).unwrap();
        let handle = driven.handle();
        handle.put(key(1), Kind::Machine, code.clone()).unwrap();
        handle.put(key(2), Kind::Query, b"a query's result".to_vec()).unwrap();
        driven.flush().unwrap();
        let pinned = handle.pinned(&key(1)).unwrap().unwrap();
        let file = pinned.file().expect("committed machine code is a file of its own");
        assert_eq!(std::fs::read(handle.path_of(file)).unwrap(), code);
    }
    let mut driven = Driven::open(&root).unwrap();
    let handle = driven.handle();
    let pinned = handle.pinned(&key(1)).unwrap().unwrap();
    assert_eq!(&*pinned, code.as_slice());
    assert_eq!(std::fs::read(handle.path_of(pinned.file().unwrap())).unwrap(), code);
    assert_eq!(handle.read(&key(2), |it| it.bytes.to_vec()).unwrap().unwrap(), b"a query's result");
    drop((pinned, handle, driven));
    _ = std::fs::remove_dir_all(&root);
}

/// A process has one installed handle: the first, which [`Handle::installed`] gives; a second
/// is refused, naming the first.
#[test]
fn a_process_installs_one_handle() {
    let mut sim = Sim::new(36, quiet());
    let mut store = open(&mut sim, "store");
    let first = Handle::install(Handle::new(&mut store, "/first".into())).unwrap();
    assert!(std::ptr::eq(first, Handle::installed().unwrap()));
    let refused = Handle::install(Handle::new(&mut store, "/second".into())).err().unwrap();
    assert!(refused.contains("/first"), "{refused}");
}
