//! Each pack's block checks (`checks-<n>`, [`crate::checks`]) through commits, IO failures and
//! crashes, on the simulation: what the head names is durable, bytes and checks, before the head
//! is (docs/v2/plan/design/store-pool.md, sections 3 and 7).
//!
//! The planted bug (`Store::plant_head_before_syncs`, the design's bug 7) writes the head once a
//! commit's writes have completed, without waiting for the syncs of its packs and their checks:
//! a crash then leaves a head naming bytes no sync made durable.

use std::collections::BTreeMap;

use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// One round of a run: puts, then a flush the power may go out in the middle of.
#[derive(Clone, Copy, Debug)]
struct Round {
    puts: u64,
    /// Polls of the flush's effects before the crash: few cut the commit short, many let it end.
    polls: u64,
    /// Whether a settle runs before the flush.
    settles: bool,
}

#[derive(Clone, Debug)]
struct Case {
    seed: u64,
    /// An effect fails once in this many (0: never).
    fail_one_in: u32,
    rounds: Vec<Round>,
}

fn draw_case(tc: &TestCase) -> Case {
    let rounds = tc.draw(gs::integers::<u8>().min_value(1).max_value(4));
    Case {
        seed: tc.draw(gs::integers()),
        fail_one_in: [0, 9, 4][usize::from(tc.draw(gs::integers::<u8>().max_value(2)))],
        rounds: (0..rounds)
            .map(|_| Round {
                puts: tc.draw(gs::integers::<u64>().min_value(1).max_value(24)),
                polls: tc.draw(gs::integers::<u64>().max_value(40)),
                settles: tc.draw(gs::integers::<u8>().max_value(3)) == 0,
            })
            .collect(),
    }
}

/// Whether `store` stopped on failures the simulation injected, `FAILURES_MOST` in a row: as on
/// a failing disk, and no finding.
fn stopped_on_injected_failures(store: &Store) -> bool {
    store.broken().is_some_and(|it| it.what.contains("injected by the simulation"))
}

/// The oracle's rule for the checks: of every own pack of `store` (open, with no commit in
/// flight), the checks file holds the checks of the blocks the head names, and the store holds
/// the same.
fn checks_hold(sim: &Sim, store: &Store) -> Result<(), String> {
    let own = &store.tiers[0];
    for (number, pack) in own.packs.iter().enumerate().filter(|(_, it)| !it.removed) {
        let bytes = sim.contents(&pack.file);
        let Some(committed) = bytes.get(..pack.committed as usize) else {
            return Err(format!(
                "{} is {} bytes, shorter than the {} its head names",
                pack.file,
                bytes.len(),
                pack.committed
            ));
        };
        let want = checks::of(committed);
        if pack.checks != want {
            return Err(format!(
                "the checks the store holds of {} are not those of its committed blocks",
                pack.file
            ));
        }
        let file = own.file(&disk::checks_file(number));
        let (on_disk, held) = (sim.contents(&file), checks::encode(&want));
        if on_disk.get(..held.len()) != Some(&held[..]) {
            return Err(format!(
                "{file} does not hold the checks of the {} blocks of its pack that its head names",
                want.len()
            ));
        }
    }
    Ok(())
}

/// The oracle's rule for the values: every value put reads back its bytes or is absent, and one
/// whose put was reported durable is there.
pub(super) fn values_hold(
    store: &Store,
    expected: &BTreeMap<Key, Vec<u8>>,
    durable: &[Key],
) -> Result<(), String> {
    for (key, bytes) in expected {
        match store.get(key) {
            Ok(Some(found)) if found.bytes == &bytes[..] => {}
            Ok(Some(_)) => return Err(format!("{key:?} reads other bytes than were put")),
            Ok(None) if durable.contains(key) => {
                return Err(format!("{key:?}, whose put was reported durable, was lost"));
            }
            Ok(None) => {}
            Err(corrupt) => return Err(format!("a value that was put is refused: {corrupt}")),
        }
    }
    Ok(())
}

/// The store opened as the daemon opens it after a crash: recovery. `Ok(None)` when injected
/// failures stopped it.
fn reopen(sim: &mut Sim) -> Result<Option<Store>, String> {
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(sim, &mut store, &mut effects);
    if stopped_on_injected_failures(&store) {
        return Ok(None);
    }
    match store.broken() {
        Some(broken) => Err(format!("the store did not open: {broken}")),
        None if !store.ready() => Err("the store did not open, and says nothing of why".into()),
        None => Ok(Some(store)),
    }
}

/// Runs `case`: rounds of puts and a flush, a crash in each, and the oracle after every reopen.
/// `Err` says what the oracle found.
fn crashes(case: &Case, plant: bool) -> Result<(), String> {
    let faults = Faults { fail_one_in: case.fail_one_in, ..Faults::default() };
    let mut sim = Sim::new(case.seed, faults);
    let mut expected: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
    let mut durable: Vec<Key> = Vec::new();
    let mut next = 0u64;
    // A last round of nothing, so the last crash is looked at too.
    let last = Round { puts: 0, polls: 0, settles: false };
    for (round, it) in case.rounds.iter().chain([&last]).enumerate() {
        let said = |why: String| format!("round {round}: {why}");
        let Some(mut store) = reopen(&mut sim).map_err(said)? else {
            sim.crash();
            continue;
        };
        store.plant_head_before_syncs = plant;
        checks_hold(&sim, &store).map_err(said)?;
        values_hold(&store, &expected, &durable).map_err(said)?;
        let mut tickets = BTreeMap::new();
        for _ in 0..it.puts {
            next += 1;
            let (kind, bytes) = match next % 4 {
                0 => (Kind::Run, run_output(next)),
                1 => (Kind::Code, noise(200 + (next as usize * 37) % 3000, next)),
                2 => (Kind::Query, noise(9000, next)),
                _ => (Kind::Query, noise(64, next)),
            };
            let ticket =
                store.put(key(next), kind, &bytes).map_err(|it| said(format!("{it:?}")))?;
            tickets.insert(ticket, key(next));
            expected.insert(key(next), bytes);
        }
        let mut effects = Vec::new();
        if it.settles {
            store.settle(&mut effects);
            for ticket in drive(&mut sim, &mut store, &mut effects) {
                durable.extend(tickets.get(&ticket).copied());
            }
        }
        store.flush(&mut effects);
        for _ in 0..it.polls {
            sim.submit(&mut effects);
            let mut out = Vec::new();
            sim.poll(&mut out, blaze_core::wait::Deadline::NEVER);
            for completion in out {
                if store.owns(completion.tag) {
                    for ticket in store.complete(completion, &mut effects) {
                        durable.extend(tickets.get(&ticket).copied());
                    }
                }
            }
        }
        if let Some(broken) = store.broken()
            && !stopped_on_injected_failures(&store)
        {
            return Err(said(format!("the store stopped: {broken}")));
        }
        sim.crash();
    }
    Ok(())
}

/// The invariant: after a crash at any point of any number of commits, under injected IO
/// failures, the store opens; each pack's checks file holds the checks of the blocks its head
/// names, and the store holds the same; every put reported durable reads back the bytes it was
/// given; and no value reads back other bytes than were put. `HEGEL_TEST_CASES=<n>` runs it
/// longer.
#[test]
fn a_crash_leaves_every_pack_and_its_checks_as_the_head_names_them() {
    super::model_check::run_cases(48, |tc| {
        let case = draw_case(&tc);
        if let Err(why) = crashes(&case, false) {
            panic!("{case:?}: {why}");
        }
    });
}

/// The same oracle over fixed seeds, so every run of the suite crosses the same crashes.
#[test]
fn the_checks_and_the_values_survive_a_crash_in_each_of_many_commits() {
    for seed in 0..60 {
        let round = |n: u64| Round {
            puts: 4 + (seed + n) % 20,
            polls: (seed * 7 + n * 13) % 40,
            settles: seed % 3 == 2,
        };
        let fail_one_in = [0, 9][(seed % 2) as usize];
        let case = Case { seed, fail_one_in, rounds: (0..4).map(round).collect() };
        assert_eq!(crashes(&case, false), Ok(()), "{case:?}");
    }
}

/// The directed case of the design's planted bug 7: one commit durable; then a put, and a flush
/// on a disk that stopped answering syncs (its writes complete, its syncs never do); then the
/// power goes out. A store that waits for its syncs has written no head, so the put was never
/// reported durable and the reopen reads the head before it. `Err` says what the oracle found.
fn head_then_crash(seed: u64, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    let first = noise(3000, 1);
    store.put(key(1), Kind::Query, &first).unwrap();
    flush(&mut sim, &mut store);
    store.plant_head_before_syncs = plant;
    let second = noise(20_000, seed);
    let ticket = store.put(key(2), Kind::Query, &second).unwrap();
    sim.hold_syncs(name("store"));
    let mut effects = Vec::new();
    store.flush(&mut effects);
    let told = drive(&mut sim, &mut store, &mut effects);
    let reported = told.contains(&ticket) || store.durable(ticket);
    sim.crash();
    drop(store);
    let store = reopen(&mut sim)?.ok_or("the store stopped on failures no one injected")?;
    checks_hold(&sim, &store)?;
    let expected = BTreeMap::from([(key(1), first), (key(2), second)]);
    let durable: Vec<Key> = [key(1)].into_iter().chain(reported.then_some(key(2))).collect();
    values_hold(&store, &expected, &durable)
}

/// The directed case over seeds: each passes with nothing planted, and the first that fails
/// with the bug planted is given, with what the oracle found.
pub(super) fn head_before_syncs_caught() -> (u64, String) {
    let mut caught = None;
    for seed in 0..32 {
        let unplanted = head_then_crash(seed, false);
        assert_eq!(unplanted, Ok(()), "seed {seed}: the store as it is must pass");
        if caught.is_none()
            && let Err(why) = head_then_crash(seed, true)
        {
            caught = Some((seed, why));
        }
    }
    caught.expect("the oracle missed the planted head written before its syncs")
}

/// Whether a store that did not open says its head names bytes that are not on the disk:
/// said by the open's own scan ("its head names ..."), or, since packs are read through the
/// pool, by the pool's load of the pack, which got fewer bytes than the head commits.
pub(super) fn names_what_is_not_there(why: &str) -> bool {
    why.contains("its head names") || (why.contains(" bytes of the ") && why.contains(" committed there"))
}

/// The planted-bug control (bug 7): with the head written before the syncs of its packs and
/// their checks complete, the directed case fails under some seed's crash, and fails as that
/// bug does: the store does not open, because its head names bytes of a pack, or checks of
/// them, that are not on disk. The same cases pass with nothing planted.
#[test]
fn the_oracle_catches_a_head_written_before_its_syncs() {
    let (seed, why) = head_before_syncs_caught();
    assert!(
        why.contains("did not open") && names_what_is_not_there(&why),
        "seed {seed}: caught, but not as a head naming what is not durable: {why}"
    );
}

/// Each pack is open in the store's pool as far as it is committed, from its tier's opening and
/// through every commit, and a removed pack's windows go with it.
#[test]
fn every_pack_is_open_in_the_pool_as_far_as_it_is_committed() {
    let mut sim = Sim::new(8, quiet());
    let mut store = open(&mut sim, "store");
    assert_eq!(store.hot.pack_open(0, 0), None, "no pack yet");
    let open_as_committed = |store: &Store| {
        for (number, pack) in store.tiers[0].packs.iter().enumerate() {
            let want = (!pack.removed).then_some(pack.committed);
            assert_eq!(store.hot.pack_open(0, number), want, "{}", pack.file);
        }
    };
    for round in 0..3u64 {
        for n in 0..6 {
            store.put(key(round * 10 + n), Kind::Code, &noise(40_000, round * 10 + n)).unwrap();
        }
        flush(&mut sim, &mut store);
        assert!(store.broken().is_none(), "{:?}", store.broken());
        open_as_committed(&store);
    }
    assert!(store.tiers[0].packs.len() > 1, "the puts fill more than one pack");
    drop(store);
    let mut store = open(&mut sim, "store");
    open_as_committed(&store);
    store.hot.pack_removed(0, 0);
    assert_eq!(store.hot.pack_open(0, 0), None);
}

/// A pump that must not be asked: a read of pages in memory loads nothing.
struct NoPump;

impl Pump for NoPump {
    fn pump(
        &mut self,
        effects: &mut Vec<Effect>,
        _done: &mut dyn FnMut(Completion) -> Option<Completion>,
    ) -> Result<(), PumpError> {
        panic!("a read of pages in memory asked for {effects:?}");
    }
}

/// A pumped miss: a pack's bytes pinned through the pool are the file's (in memory from the
/// commit that wrote them through the pool, a hit); a second read is a hit and asks nothing;
/// given back, they load again, by reads the caller's pump carries out on the simulation,
/// checked by block; and a block rotted on disk meanwhile is refused,
/// naming the pack and the block, never served.
#[test]
fn a_pumped_miss_loads_a_packs_bytes_checked_and_again_once_given_back() {
    let mut sim = Sim::new(9, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..5 {
        store.put(key(n), Kind::Code, &noise(40_000, n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let (file, committed) = {
        let pack = &store.tiers[0].packs[0];
        (pack.file.clone(), pack.committed)
    };
    let on_disk = sim.contents(&file);
    assert!(committed > 2 * crate::pool::PAGE, "the pack is more than two pages");
    for round in 0..2 {
        {
            let mut pump = Inline::new(&mut sim);
            let pins = store
                .hot
                .pin_pack(&mut pump, 0, 0, 0, committed)
                .unwrap_or_else(|err| panic!("round {round}: {err}"));
            assert_eq!(pins.bytes(), &on_disk[..committed as usize], "round {round}");
            drop(pins);
            assert_eq!(pump.strays().len(), 0, "round {round}: only the loads' reads were in flight");
        }
        {
            let hit = store.hot.pin_pack(&mut NoPump, 0, 0, 70_000, 300).unwrap();
            assert_eq!(hit.bytes(), &on_disk[70_000..70_300], "round {round}");
        }
        let swept = store.hot.evict(u64::MAX).expect("the store has a pool");
        // All but the pack's last page, which the sweep passes over while the pack was
        // written in the last two commits.
        let given = swept.freed >= committed - crate::pool::PAGE;
        assert_eq!((swept.refused, given), (None, true), "round {round}: {}", swept.freed);
    }
    sim.rot(&file, 70_000, &[0xff; 8]);
    let mut pump = Inline::new(&mut sim);
    let refused = store.hot.pin_pack(&mut pump, 0, 0, 0, committed).map(|_| ()).unwrap_err();
    let block = 70_000 / disk::BLOCK;
    let said = refused.to_string();
    assert!(
        said.contains("pack-000000") && said.contains(&format!("block {block} ")),
        "{refused:?}"
    );
    drop(pump);
    let past = store.hot.pin_pack(&mut NoPump, 0, 0, committed - 1, 2).map(|_| ()).unwrap_err();
    assert!(matches!(past, crate::hot::PinError::NotCommitted { .. }), "{past:?}");
}

/// A block of a pack changed on disk under what was committed (rot) stops the store as it
/// opens, naming the pack and the block: the recovery scan checks every block it reads.
#[test]
fn a_changed_block_of_a_pack_stops_the_store_naming_the_pack_and_the_block() {
    let mut sim = Sim::new(4, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, &[b'a'; 1000]).unwrap();
    store.put(key(2), Kind::Query, &[b'b'; 1000]).unwrap();
    flush(&mut sim, &mut store);
    let loc = store.loc_of(&key(2));
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    let at = (loc.offset + RECORD_HEADER) as usize + 500;
    bytes[at] ^= 1;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    let broken = store.broken().expect("the block is checked at open");
    assert_eq!(broken.file, pack);
    let block = at as u64 / disk::BLOCK;
    assert!(
        broken.what.contains(&format!("block {block} ")) && broken.what.contains("checks-000000"),
        "{broken}"
    );
}

/// A checks file that holds fewer checks than the head's committed length has blocks stops the
/// store, naming it: nothing is read unchecked.
#[test]
fn a_pack_with_fewer_checks_than_its_head_names_stops_the_store_naming_them() {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &noise(9000, 1)).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let checks_file = name("store/checks-000000");
    let whole = sim.contents(&checks_file);
    let pack = sim.contents(&name("store/pack-000000"));
    assert_eq!(whole.len() as u64, checks::len_of(pack.len() as u64));
    sim.change(&checks_file, &whole[..whole.len() - 1]);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    let broken = store.broken().expect("the checks are counted at open");
    assert_eq!(broken.file, checks_file);
    assert!(broken.what.contains("fewer than"), "{broken}");
}

/// Removing a pack's file removes its checks with it: what the commit that retires a pack asks
/// for, and what the next open asks again of every pack its head names removed.
#[test]
fn a_removed_packs_checks_are_removed_with_it() {
    let mut sim = Sim::new(6, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &noise(700, 1)).unwrap();
    flush(&mut sim, &mut store);
    assert!(sim.exists(&name("store/checks-000000")));
    let mut effects = Vec::new();
    store.remove_pack(0, &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!sim.exists(&name("store/pack-000000")) && !sim.exists(&name("store/checks-000000")));
    assert_eq!(store.problems(), &[] as &[String]);
}

/// A reopened store asks for no mapping of a pack: what its packs commit is loaded into its
/// pool by reads its driver carries out, each block checked, and every value is read from
/// there, its pages pinned by the readers' table until the store lets the table go.
#[test]
fn a_store_reads_its_packs_through_its_pool_and_maps_none() {
    let mut sim = Sim::new(21, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..5 {
        store.put(key(n), Kind::Code, &noise(40_000, n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let committed = store.tiers[0].packs[0].committed;
    drop(store);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let (mut maps, mut reads) = (0, 0);
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        maps += effects.iter().filter(|it| matches!(it, Effect::Map { .. })).count();
        reads += effects.iter().filter(|it| matches!(it, Effect::ReadInto { .. })).count();
    })
    .expect("the effects end");
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(maps, 0, "no pack is mapped");
    assert!(reads >= 1, "the pack was read into the pool");
    let census = store.pool_census().expect("the store has a pool");
    assert!(census.loaded >= committed, "{census:?}: the pack commits {committed}");
    for n in 0..5 {
        assert_eq!(bytes_of(&store, &key(n)), Some(noise(40_000, n)), "{n}");
    }
    let pinned = store.pool_census().expect("the store has a pool").pinned;
    assert!(pinned >= crate::pool::PAGE, "the reads' pages are pinned by the readers' table: {pinned}");
    // A value held past its task has pins of its own.
    let held = store.pinned(&key(1)).unwrap().expect("the value is kept");
    assert_eq!(&*held, noise(40_000, 1).as_slice());
    assert_eq!(store.verify(None), Vec::new());
}

/// A prefetch of records that are neighbours in a pack is one plan of one run, and one read:
/// given back by a sweep, a pumped store's pages are refused to a read (it can load nothing),
/// naming why; the hint's read brings them back, and the values read whole.
#[test]
fn a_prefetch_of_neighbouring_records_is_one_run_and_one_read() {
    let mut sim = Sim::new(22, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..5 {
        store.put(key(n), Kind::Code, &noise(40_000, n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let swept = store.hot.evict(u64::MAX).expect("the store has a pool");
    assert!(swept.freed >= 2 * crate::pool::PAGE && swept.refused.is_none(), "{swept:?}");
    let refused = store.get(&key(0)).expect_err("its page is not in memory, and nothing loads it");
    assert!(refused.what.contains("not in memory") && refused.what.contains("Loader::Pumped"), "{refused}");
    let keys: Vec<Key> = (0..4).map(key).collect();
    let mut effects = Vec::new();
    let hint = store.prefetch(&keys, &mut effects);
    assert_eq!((hint.runs, hint.handed), (1, true), "{hint:?}");
    let reads = effects.iter().filter(|it| matches!(it, Effect::ReadInto { .. })).count();
    assert_eq!(reads, 1, "{} records, one read: {effects:?}", keys.len());
    drive(&mut sim, &mut store, &mut effects);
    for n in 0..5 {
        assert_eq!(bytes_of(&store, &key(n)), Some(noise(40_000, n)), "{n}");
    }
}

/// A read at the pinned bound waits, a tick at a time, for the store to let pins go, and ends
/// in a typed refusal naming the bound when none are: never a wait without end, and never a
/// pin past the bound.
#[test]
fn a_read_at_the_pinned_bound_waits_its_ticks_and_is_refused_naming_the_bound() {
    let mut sim = Sim::new(23, quiet());
    let store = open(&mut sim, "store");
    let page = crate::pool::PAGE;
    assert_eq!(store.shared.pin_room_within(page, u64::MAX, 2), Ok(()), "under the bound: no wait");
    let refused = store.shared.pin_room_within(page, 0, 2).expect_err("nothing fits under a bound of 0");
    assert_eq!((refused.bound, refused.need), (0, page), "{refused}");
    assert!(refused.to_string().contains("its bound is 0"), "{refused}");
}

/// A store whose misses its loader's thread loads ([`Loader::Thread`]), on a real file
/// system: reopened, it reads what its opening's scan and its readers ask for through the
/// pool; given back by a sweep (the pins of ended tasks let go first), its pages load again
/// as they are read, each block checked; and no pack is mapped at any point.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_store_with_a_loader_loads_what_is_read_and_again_once_it_is_given_back() {
    let held = blaze_core::temp::TempDir::for_test("blaze-cas-loader-");
    let root = held.path().to_path_buf();
    let loader = || Loader::Thread { root: root.clone() };
    {
        let mut driven = Driven::open_with(&root, loader()).unwrap();
        for n in 0..5 {
            driven.store().put(key(n), Kind::Code, &noise(40_000, n)).unwrap();
        }
        driven.flush().unwrap();
        for n in 0..5 {
            assert_eq!(bytes_of(driven.store(), &key(n)), Some(noise(40_000, n)), "{n}: as it was put");
        }
    }
    let mut driven = Driven::open_with(&root, loader()).unwrap();
    for round in 0..2 {
        for n in 0..5 {
            assert_eq!(bytes_of(driven.store(), &key(n)), Some(noise(40_000, n)), "round {round}: {n}");
        }
        let before = driven.store().pool_census().expect("the store has a pool");
        // The pins of the reads above go once their grace has passed: other tests of this
        // process hold guards of their own meanwhile, so the sweep is asked until it has.
        let mut freed = 0;
        for _ in 0..10_000 {
            freed += driven.store().evict_pool(u64::MAX);
            if freed >= before.loaded {
                break;
            }
            std::thread::yield_now();
        }
        let after = driven.store().pool_census().expect("the store has a pool");
        assert_eq!(
            (freed, after.loaded, after.pinned),
            (before.loaded, 0, 0),
            "round {round}: every page is given back once no task pins it: {before:?} before, {after:?} after"
        );
    }
    drop(driven);
}
