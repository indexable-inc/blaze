//! One home for a placed output ([`crate::home`]): once its placed file is durable, the value's
//! record names the file and the pack keeps none of its bytes. Crashes at every step are
//! `place_crash`'s; these say what a run with no crash leaves, what the answer waits for, what
//! an open does with a home, how a home goes, and how a head lists one.

use super::*;

/// The output under key 1, of `CLONE_FROM + 9000` bytes (placed by cloning, kept in a pack
/// whole), put and placed at `one/build/a` with `plant`, everything driven to its end and
/// flushed: the store, the bytes and the placed file's name.
fn homed(sim: &mut Sim, plant: PlacePlant, seed: u64) -> (Store, Vec<u8>, FileName) {
    let mut store = open(sim, "store");
    store.plant = plant;
    let out = noise(CLONE_FROM as usize + 9000, seed);
    store.put(key(1), Kind::Output, &out).unwrap();
    flush(sim, &mut store);
    assert_eq!(place_key(sim, &mut store, key(1), "one/build/a").result, Ok(()));
    flush(sim, &mut store);
    let file = name(&format!("store/placed/{:x}", content_of(&out)));
    (store, out, file)
}

/// The bytes the store's own records of outputs keep in its packs.
fn output_bytes(store: &Store) -> u64 {
    store.census().kinds[Kind::Output.code() as usize].stored
}

/// The invariant: once a placed output's file is synced and the commit after it is durable,
/// its bytes are on disk once: the file, which the census counts as a whole file of the
/// store's, and no bytes of it in a pack; the record reads the value through the file. The
/// control (`HomesNothing`): the pack keeps them too, every byte twice.
#[test]
fn a_placed_outputs_bytes_are_on_disk_once_after_its_file_is_made_its_home() {
    for seed in 0..8 {
        let mut sim = Sim::new(500 + seed, quiet());
        let (store, out, file) = homed(&mut sim, PlacePlant::None, seed);
        let len = out.len() as u64;
        assert!(sim.exists(&file));
        assert_eq!(output_bytes(&store), 0, "seed {seed}: the pack keeps no byte of it");
        assert_eq!(store.census().whole_files, (1, len), "seed {seed}: the file is its home");
        assert_eq!(bytes_of(&store, &key(1)).unwrap(), out, "seed {seed}: read through the file");
        assert_eq!(store.problems(), &[] as &[String]);

        let mut sim = Sim::new(500 + seed, quiet());
        let (store, out, _) = homed(&mut sim, PlacePlant::HomesNothing, seed);
        assert!(
            output_bytes(&store) >= out.len() as u64 / 2,
            "seed {seed}: the control keeps the bytes in the pack as well"
        );
        assert_eq!(store.census().whole_files, (0, 0));
    }
}

/// The invariant: making a file a home costs a place's answer nothing: the place is answered
/// before the store asks for the sync of its file, so an edit's answer pays no fsync. The
/// syncs come after, and the record moves only once both answered.
#[test]
fn a_place_is_answered_before_its_file_is_synced_as_a_home() {
    for seed in 0..8 {
        let mut sim = Sim::new(600 + seed, quiet());
        let mut store = open(&mut sim, "store");
        let out = noise(CLONE_FROM as usize + 9000, seed);
        store.put(key(1), Kind::Output, &out).unwrap();
        flush(&mut sim, &mut store);
        let file = name(&format!("store/placed/{:x}", content_of(&out)));
        // Synced under its writing name before it takes its content's ([`Store::name_staged`]).
        let writing = name(&format!("store/placed/{:x}{WRITING_SUFFIX}", content_of(&out)));
        let mut effects = Vec::new();
        let tag = store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
        let (mut answered, mut synced_before, mut synced) = (false, false, false);
        let mut done = Vec::new();
        let end = blaze_core::wait::Deadline::after(sim.now(), Duration::from_secs(600));
        loop {
            for effect in &effects {
                if let Effect::Sync { file: it, .. } = effect
                    && (*it == file || *it == writing)
                {
                    synced = true;
                    synced_before |= !answered;
                }
            }
            sim.submit(&mut effects);
            if sim.in_flight() == 0 {
                break;
            }
            sim.poll(&mut done, end);
            assert!(!done.is_empty(), "seed {seed}: the effects end");
            for completion in done.drain(..) {
                if store.owns(completion.tag) {
                    store.complete(completion, &mut effects);
                }
            }
            for placed in store.placed() {
                assert_eq!((placed.tag, placed.result), (tag, Ok(())));
                answered = true;
            }
        }
        assert!(answered && synced, "seed {seed}: answered {answered}, synced {synced}");
        assert!(!synced_before, "seed {seed}: the answer waited for the home's sync");
        flush(&mut sim, &mut store);
        assert_eq!(output_bytes(&store), 0, "seed {seed}");
    }
}

/// The invariant: a home is the record's bytes across an open: the head names it, the open's
/// sweep of `placed/` keeps it though no path is known to name it, the value reads through
/// it, and a place of it again clones it, writing nothing.
#[test]
fn a_home_is_kept_across_an_open_and_cloned_from_again_unwritten() {
    let mut sim = Sim::new(17, quiet());
    let (store, out, file) = homed(&mut sim, PlacePlant::None, 17);
    drop(store);
    let mut store = open(&mut sim, "store");
    let len = out.len() as u64;
    assert_eq!(store.placed_on_open(), PlacedOnOpen { kept: 1, kept_bytes: len, swept: 0 });
    assert!(sim.exists(&file));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), out);
    assert_eq!(output_bytes(&store), 0);
    let placed = place_key(&mut sim, &mut store, key(1), "two/build/a");
    assert_eq!(placed.result, Ok(()));
    assert_eq!(placed.bytes.written, 0, "cloned from its home: {:?}", placed.bytes);
    assert_eq!(placed.bytes.cloned, len);
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert_eq!(store.problems(), &[] as &[String]);
}

/// The invariant: a home goes with its record, never alone. While no path names it but its
/// key is not yet forgotten, the file stays (the record's bytes are in it); once nothing names
/// the key, the forget removes the record and the head after it the file. The control: the
/// file is there, and the value read, before the forget.
#[test]
fn a_home_goes_by_its_records_forget_and_never_alone() {
    let mut sim = Sim::new(19, quiet());
    let (mut store, out, file) = homed(&mut sim, PlacePlant::None, 19);
    store.put(key(2), Kind::Output, b"another value").unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(place_key(&mut sim, &mut store, key(2), "one/build/a").result, Ok(()));
    flush(&mut sim, &mut store);
    assert!(sim.exists(&file), "the control: no path names it, and it is still its record's home");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), out);
    // A place over it asks no forget (D45): it is asked here, as the collector would.
    assert_eq!(forgets_due(&mut sim, &mut store).forgotten, Vec::<Key>::new());
    store.forget(key(1));
    let forgot = forgets_due(&mut sim, &mut store);
    assert_eq!(forgot.forgotten, vec![key(1)]);
    flush(&mut sim, &mut store);
    assert!(bytes_of(&store, &key(1)).is_none());
    assert!(!sim.exists(&file), "the head that forgot it is durable: its file goes");
    drop(store);
    let store = open(&mut sim, "store");
    assert!(bytes_of(&store, &key(1)).is_none(), "forgotten for good");
    assert_eq!(store.problems(), &[] as &[String]);
}

/// The invariant (rule 26): a head of format 7, which lists no home, reads as it was written,
/// every file it lists a `file-<content>` one; a head of format 8 lists homes and homes
/// removed apart from the others, and round-trips them; a format-7 length that collides with
/// the home bit is refused by name.
#[test]
fn a_head_of_format_7_reads_with_no_homes_and_one_of_format_8_round_trips_them() {
    let (a, b, c) = (content_of(b"a"), content_of(b"b"), content_of(b"c"));
    let mut head = Head {
        generation: 3,
        index: None,
        packs: vec![],
        dictionaries: vec![],
        externals: vec![
            ExternalRef { content: a, len: 70 << 10, placed: false },
            ExternalRef { content: b, len: disk::EXTERNAL_REMOVED, placed: false },
        ],
        units: vec![],
        forgotten: vec![],
    };
    let old = head.encode_as(7);
    assert_eq!(old[8..12], 7u32.to_le_bytes());
    assert_eq!(Head::decode(&old).unwrap(), head, "a head of format 7 lists no home");
    head.externals.push(ExternalRef { content: c, len: 40 << 10, placed: true });
    head.externals.push(ExternalRef { content: content_of(b"d"), len: disk::EXTERNAL_REMOVED, placed: true });
    assert_eq!(FORMAT, 8);
    assert_eq!(Head::decode(&head.encode()).unwrap(), head);
    let mut collides = head.clone();
    collides.externals.truncate(1);
    collides.externals[0].len |= disk::EXTERNAL_PLACED;
    let why = Head::decode(&collides.encode_as(7)).unwrap_err();
    assert!(why.contains(&format!("{a:x}")), "{why}");
}
