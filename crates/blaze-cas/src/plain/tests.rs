//! The plain store under the deterministic simulation (`blaze_core::sim::Sim`): two writers
//! interleaved on one disk, power cuts at any completion (each file keeping a random prefix of
//! what was not synced, torn at a sector), failed effects, and a reader of a new boot after.
//!
//! The invariant: the open after a power cut reads a whole generation `w` ([`Report::whole`]),
//! or none; every record committed at or before `w` reads back byte for byte or its segment
//! is reported lost by name; a record committed after `w` (no sync on the edit path, so a cut
//! can take every byte of it) reads back whole or not at all; a record whose commit had not
//! completed reads back whole or not at all; a key never put is never found; no read returns
//! a corrupt record; and with no power cut (in durable mode, with one too) nothing is lost and
//! every committed record reads back.
//!
//! [`Report::whole`]: super::Report::whole
//!
//! The controls (`the_oracle_catches_*`) plant one bug each and show the oracle fails.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use hegel::generators as gs;

use super::{
    core::{Plain, PlainConfig, Plant, TAGS},
    driven::{Driven, Found, LocalDir, Source, Verifier},
    format::{self, Namespace},
};
use crate::{Key, Kind};

/// A tag no store owns: the test's own asks (a clock, a listing).
const OWN: u64 = 1 << 60;

fn dir() -> FileName {
    FileName::new("plain").expect("a name")
}

fn cfg(n: u64, boot: u64, durable: bool) -> PlainConfig {
    PlainConfig {
        dir: dir(),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 100 + n,
        durable,
        page: 16 << 10,
        namespace: Namespace::LOCAL,
        reads: vec![Namespace::LOCAL, Namespace::PUBLIC],
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain store test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    let len = (n as usize * 37) % 700;
    (0..len).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

struct Actor {
    store: Plain,
    effects: Vec<Effect>,
}

impl Actor {
    fn new(cfg: PlainConfig, plant: Plant) -> Actor {
        let mut store = Plain::planted(cfg, plant);
        let mut effects = Vec::new();
        store.open(&mut effects);
        Actor { store, effects }
    }
}

/// Runs until nothing is in flight, or `budget` completions have been taken (then `false`: the
/// caller cuts the power). Bounded: a run that does not settle in a million polls fails.
fn run(sim: &mut Sim, actors: &mut [Actor], budget: &mut Option<u32>) -> bool {
    let mut done: Vec<Completion> = Vec::new();
    for _ in 0..1_000_000 {
        for actor in actors.iter_mut() {
            sim.submit(&mut actor.effects);
        }
        if sim.in_flight() == 0 {
            return true;
        }
        let deadline = Deadline::after(sim.now(), Duration::from_secs(1));
        sim.poll(&mut done, deadline);
        let wall = sim.wall_ns();
        for completion in done.drain(..) {
            if completion.tag.0 >= OWN {
                continue;
            }
            if let Some(left) = budget {
                if *left == 0 {
                    return false;
                }
                *left -= 1;
            }
            let actor = actors.iter_mut().find(|it| it.store.owns(completion.tag)).expect("a store asked for it");
            actor.store.complete(completion, wall, &mut actor.effects);
        }
    }
    panic!("the simulation did not settle in a million polls");
}

fn settle(sim: &mut Sim, actors: &mut [Actor]) {
    assert!(run(sim, actors, &mut None));
}

/// One effect of the test's own, run to its completion.
fn ask(sim: &mut Sim, effect: Effect) -> Done {
    let mut effects = vec![effect];
    let mut done = Vec::new();
    for _ in 0..10_000 {
        sim.submit(&mut effects);
        if sim.in_flight() == 0 {
            break;
        }
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
    }
    done.pop().expect("its completion").done
}

/// Moves the simulation's clocks on by `after`.
fn wait(sim: &mut Sim, after: Duration) {
    assert!(matches!(ask(sim, Effect::Timer { after, tag: Tag(OWN) }), Done::Fired));
}

/// A reader of `boot`, opened again while an injected failure breaks its open (a new process
/// each time: the store never retries itself).
fn reader(sim: &mut Sim, n: u64, boot: u64) -> Actor {
    for _ in 0..50 {
        let mut it = [Actor::new(cfg(n, boot, false), Plant::None)];
        settle(sim, &mut it);
        let [it] = it;
        match it.store.broken() {
            None => return it,
            Some(why) => assert!(why.contains("injected"), "a reader broke on its own: {why}"),
        }
    }
    panic!("no reader opened in 50 tries");
}

#[derive(Default)]
struct Ledger {
    /// Keys whose commit completed, with the segment that holds them and the generation the
    /// commit made.
    committed: BTreeMap<u32, (u64, u64)>,
    /// Keys put, whose commit had not completed.
    maybe: BTreeSet<u32>,
    /// Keys never put.
    never: Vec<u32>,
}

/// The oracle. `cut`: a power cut happened; `durable`: the writers synced every commit, so a
/// cut may take only what no commit completed (an interrupted flip's traces, reported, are
/// right to report) and every committed record must read back.
fn oracle(store: &Plain, ledger: &Ledger, cut: bool, durable: bool) -> Result<(), String> {
    let lost: BTreeSet<u64> = store.report().lost.iter().filter_map(|it| it.segment).collect();
    // Verify once: no segment checked twice by this process, and no read checked anything.
    let mut once = BTreeSet::new();
    if let Some(twice) = store.fully_checked().iter().find(|it| !once.insert(**it)) {
        return Err(format!("segment {twice:016x} was checked in full twice in one open"));
    }
    // Without a power cut nothing committed is lost and no generation is torn; what may be
    // reported is a segment no commit completed (a commit an injected failure stopped before
    // its first flip landed), which the open finds unnamed and says so.
    let named: BTreeSet<u64> = ledger.committed.values().map(|it| it.0).collect();
    if !cut
        && let Some(it) = store.report().lost.iter().find(|it| it.segment.is_none_or(|it| named.contains(&it)))
    {
        return Err(format!("lost without a power cut: {it:?}"));
    }
    let loss = cut && !durable;
    if !loss && let Some(it) = store.report().lost.iter().find(|it| it.segment.is_some_and(|it| named.contains(&it))) {
        return Err(format!("a committed segment lost with no power cut or in durable mode: {it:?}"));
    }
    // The newest generation the open read whole: with no power cut, the newest committed.
    let whole = store.report().whole.unwrap_or(0);
    for (&n, &(segment, generation)) in &ledger.committed {
        if !loss && generation > whole {
            return Err(format!("committed record {n} of generation {generation}, and with no power cut the open read generation {whole}"));
        }
        match store.get(&key(n)) {
            Ok(Some(got)) if got.bytes == value(n) && got.kind == Kind::Query => {}
            Ok(None) if lost.contains(&segment) => {}
            // Committed after the generation that survived, with no sync: a cut may leave no
            // trace of it to name.
            Ok(None) if generation > whole => {}
            other => {
                return Err(format!(
                    "committed record {n} (segment {segment:016x}, generation {generation}; generation {whole} read whole) reads {other:?}, and no loss names it"
                ));
            }
        }
    }
    for &n in &ledger.maybe {
        match store.get(&key(n)) {
            Ok(None) => {}
            Ok(Some(got)) if got.bytes == value(n) => {}
            other => return Err(format!("record {n}, put and not committed, reads {other:?}")),
        }
    }
    for &n in &ledger.never {
        match store.get(&key(n)) {
            Ok(None) => {}
            other => return Err(format!("record {n}, never put, reads {other:?}")),
        }
    }
    if store.checks().per_read != 0 {
        return Err(format!("reads made checks: {:?}", store.checks()));
    }
    Ok(())
}

/// Two writers, `rounds` commits each of `per` records, interleaved; the power cut after
/// `crash_after` completions when given; then a reader of a new boot (or of the same, with no
/// cut) and the oracle.
fn scenario(seed: u64, rounds: u32, per: u32, crash_after: Option<u32>, fail_one_in: u32, plants: [Plant; 2], durable: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
    let mut actors = vec![Actor::new(cfg(0, 1, durable), plants[0]), Actor::new(cfg(1, 1, durable), plants[1])];
    let mut budget = crash_after;
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut next = [0u32, 1_000_000];
    let mut pending: [Option<Vec<u32>>; 2] = [None, None];
    let mut crashed = false;
    for round in 0..=rounds {
        for (i, actor) in actors.iter_mut().enumerate() {
            if round == rounds || !actor.store.ready() || pending[i].is_some() {
                continue;
            }
            let keys: Vec<u32> = (0..per).map(|k| next[i] + k).collect();
            next[i] += per;
            for &n in &keys {
                actor.store.put(key(n), Kind::Query, &value(n)).map_err(|it| it.to_string())?;
                ledger.maybe.insert(n);
            }
            actor.store.commit(Vec::new(), sim.wall_ns(), &mut actor.effects).map_err(|it| it.to_string())?;
            pending[i] = Some(keys);
        }
        let settled = run(&mut sim, &mut actors, &mut budget);
        for (i, actor) in actors.iter_mut().enumerate() {
            if let Some(Ok(committed)) = actor.store.take_committed() {
                for n in pending[i].take().expect("a commit was pending") {
                    ledger.maybe.remove(&n);
                    ledger.committed.insert(n, (committed.segment.expect("a commit of records writes a segment"), committed.generation));
                }
            }
        }
        if !settled {
            sim.crash();
            crashed = true;
            break;
        }
        if let Some(why) = actors.iter().find_map(|it| it.store.broken()) {
            if !why.contains("injected") {
                return Err(format!("a writer broke on its own: {why}"));
            }
        }
    }
    drop(actors);
    let boot = if crashed { 2 } else { 1 };
    let r = reader(&mut sim, 2, boot);
    oracle(&r.store, &ledger, crashed, durable)?;
    // Reopened again in the same boot: what the first reader read, byte for byte, with no
    // loss reported a second time.
    let again = reader(&mut sim, 3, boot);
    for &n in ledger.committed.keys().chain(ledger.maybe.iter()) {
        let (a, b) = (r.store.get(&key(n)), again.store.get(&key(n)));
        if a.as_ref().map(|it| it.map(|it| it.bytes)) != b.as_ref().map(|it| it.map(|it| it.bytes)) {
            return Err(format!("record {n} reads {a:?}, and {b:?} after a reopen"));
        }
    }
    if !again.store.report().lost.is_empty() {
        return Err(format!("a second open reported again: {:?}", again.store.report().lost));
    }
    // The first open of this boot recorded its checks: none is made twice in one boot.
    if !again.store.fully_checked().is_empty() {
        return Err(format!("segments checked in full a second time in one boot: {:x?}", again.store.fully_checked()));
    }
    Ok(())
}

/// The invariant: two writers interleaved, power cut anywhere, effects failing: every
/// committed record reads back byte for byte or is reported lost by name; nothing put and not
/// committed is half there; nothing never put is found; nothing corrupt is returned.
#[hegel::test]
fn two_writers_and_power_cuts_keep_every_committed_record_or_say_so(tc: hegel::TestCase) {
    two_writers_and_power_cuts(tc);
}

/// Regression (2026-11-13, round 3): a power cut tore the head's first write (then a whole-file
/// write of 4160 bytes) to 3072 bytes, and the reader took the short head for another store's
/// file and stopped. The head is now made at its length and synced at the first open, every
/// flip writes one slot in place, and a short head is read as far as it goes.
///
/// The history is Hegel's minimal case (blob `AXicY2MAAi5GBjjFBKEYUShRKA8ACFMAYw==`: seed 0,
/// two rounds of one record, a power cut after 21 completions, no injected failure), written
/// out step by step.
#[test]
fn a_head_torn_by_a_power_cut_is_read_through_not_refused() {
    let mut sim = Sim::new(0, Faults::default());
    // Two writers of boot 1 open the empty store: the first to read the head makes it.
    let mut actors = vec![Actor::new(cfg(0, 1, false), Plant::None), Actor::new(cfg(1, 1, false), Plant::None)];
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut budget = Some(21);
    let mut pending: [Option<u32>; 2] = [None, None];
    let mut next = [0u32, 1_000_000];
    let mut crashed = false;
    // Each round, a writer whose open is done and whose last commit completed puts its next
    // record (0, 1, ... and 1_000_000, ...) and commits; then the disk runs until it settles
    // or the 21st completion, the power cut. (Round 0 finds the opens in flight: it runs them.)
    for _round in 0..2u32 {
        for (i, actor) in actors.iter_mut().enumerate() {
            if !actor.store.ready() || pending[i].is_some() {
                continue;
            }
            let n = next[i];
            next[i] += 1;
            actor.store.put(key(n), Kind::Query, &value(n)).unwrap();
            ledger.maybe.insert(n);
            actor.store.commit(Vec::new(), sim.wall_ns(), &mut actor.effects).unwrap();
            pending[i] = Some(n);
        }
        let settled = run(&mut sim, &mut actors, &mut budget);
        for (i, actor) in actors.iter_mut().enumerate() {
            if let Some(Ok(committed)) = actor.store.take_committed() {
                let n = pending[i].take().expect("a commit was pending");
                ledger.maybe.remove(&n);
                ledger.committed.insert(n, (committed.segment.expect("a segment"), committed.generation));
            }
        }
        if !settled {
            crashed = true;
            break;
        }
    }
    assert!(crashed, "the history ends before its 21st completion: the case no longer cuts it");
    drop(actors);
    sim.crash();
    // Whatever the cut left of the head (short, torn, or whole), a reader of the next boot
    // opens: the head is never taken for another store's file, and the invariant holds.
    let r = reader(&mut sim, 2, 2);
    assert_eq!(r.store.broken(), None);
    let head = sim.contents(&FileName::new("plain/head").unwrap());
    assert!(head.len() <= format::HEAD_LEN, "a head of {} bytes", head.len());
    let outcome = oracle(&r.store, &ledger, true, false);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// Regression (2026-11-13, round 4; Hegel blob `AXicY2MAAi5GIMGEQjFCKAZUCgAH4wBO`: seed 2, two
/// rounds of one record, no cut, one effect in 40 failing): a writer failed under the flip lock
/// and stopped holding it, so every reader's open that had to flip (to name an orphan) waited
/// on it, asking again every 2 ms, until an injected failure of one ask ended it, 50 times
/// over (the diagnosis read from the code, not from a trace of this case). A failure under
/// the lock now lets it go.
#[test]
fn a_store_that_fails_under_the_flip_lock_lets_it_go() {
    let outcome = scenario(2, 2, 1, None, 40, [Plant::None, Plant::None], false);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// The same small history (seed 0, two rounds of one record), cut at every completion, with
/// and without injected failures.
#[test]
fn a_small_history_cut_at_every_completion_keeps_the_invariant() {
    for fail in [0, 40] {
        for after in 0..400 {
            let outcome = scenario(0, 2, 1, Some(after), fail, [Plant::None, Plant::None], false);
            assert!(outcome.is_ok(), "cut after {after}, fail 1 in {fail}: {outcome:?}");
        }
    }
}

fn two_writers_and_power_cuts(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let rounds = tc.draw(gs::integers::<u32>().min_value(1).max_value(6));
    let per = tc.draw(gs::integers::<u32>().min_value(1).max_value(24));
    let cut = tc.draw(gs::integers::<u32>().min_value(0).max_value(1)) == 1;
    let after = tc.draw(gs::integers::<u32>().min_value(0).max_value(400));
    let fail = if tc.draw(gs::integers::<u32>().min_value(0).max_value(3)) == 0 { 40 } else { 0 };
    let outcome = scenario(seed, rounds, per, cut.then_some(after), fail, [Plant::None, Plant::None], false);
    assert!(outcome.is_ok(), "seed {seed}, {rounds} rounds of {per}, cut {cut} after {after}, fail 1 in {fail}: {outcome:?}");
}

/// Durable mode: a power cut loses nothing a completed commit wrote.
#[hegel::test]
fn durable_commits_survive_any_power_cut(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let after = tc.draw(gs::integers::<u32>().min_value(0).max_value(400));
    let outcome = scenario(seed, 4, 8, Some(after), 0, [Plant::None, Plant::None], true);
    assert!(outcome.is_ok(), "seed {seed}, cut after {after}: {outcome:?}");
}

/// The oracle fails for some seed of `seeds` with `plants`, and passes for all without.
fn caught(seeds: u64, plants: [Plant; 2], durable: bool, cut: impl Fn(u64) -> Option<u32>) {
    let mut caught = None;
    for seed in 0..seeds {
        let honest = scenario(seed, 3, 6, cut(seed), 0, [Plant::None, Plant::None], durable);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none()
            && let Err(why) = scenario(seed, 3, 6, cut(seed), 0, plants, durable)
        {
            caught = Some(why);
        }
    }
    assert!(caught.is_some(), "no seed of {seeds} caught {plants:?}");
}

#[test]
fn the_oracle_catches_a_skipped_segment_sync() {
    caught(64, [Plant::SkipDataSync, Plant::SkipDataSync], true, |seed| Some(40 + (seed as u32 * 7) % 200));
}

/// The control of durable mode's order (the segment synced before the flip that names it). The
/// plant flips first and syncs the segment after, so its bug lives only in the window between
/// the commit's report and that late sync: a drawn cut almost never lands there, so the cut is
/// placed in it, at the first completion after the commit is reported, before anything else.
/// In the mode with no sync the plant changes nothing (no sync is on that path at all, and a
/// cut may take an unsynced commit by design): the control is for durable mode alone.
#[test]
fn the_oracle_catches_a_flip_before_the_sync() {
    let cut_as_the_commit_is_told = |seed: u64, plant: Plant| -> Result<(), String> {
        let mut sim = Sim::new(seed, Faults::default());
        let mut w = [Actor::new(cfg(0, 1, true), plant)];
        settle(&mut sim, &mut w);
        for n in 0..6 {
            w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        let mut ledger = Ledger::default();
        let mut done = Vec::new();
        'run: for _ in 0..100_000 {
            sim.submit(&mut w[0].effects);
            assert!(sim.in_flight() > 0, "the commit settled without being told");
            sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for completion in done.drain(..) {
                w[0].store.complete(completion, wall, &mut w[0].effects);
                if let Some(committed) = w[0].store.take_committed() {
                    let committed = committed.expect("the commit completes");
                    let segment = committed.segment.expect("a segment");
                    ledger.committed = (0..6).map(|n| (n, (segment, committed.generation))).collect();
                    // The power cut, now: what the commit asked after its report (the plant's
                    // late sync) has not completed.
                    break 'run;
                }
            }
        }
        assert!(!ledger.committed.is_empty(), "the commit was never told");
        drop(w);
        sim.crash();
        let r = reader(&mut sim, 1, 2);
        oracle(&r.store, &ledger, true, true)
    };
    let mut caught = None;
    for seed in 0..16 {
        let honest = cut_as_the_commit_is_told(seed, Plant::None);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none() {
            caught = cut_as_the_commit_is_told(seed, Plant::FlipBeforeSync).err();
        }
    }
    assert!(caught.is_some(), "no seed of 16 caught a flip before the segment's sync in durable mode");
}

#[test]
fn the_oracle_catches_an_index_off_by_one() {
    let outcome = scenario(1, 2, 4, None, 0, [Plant::IndexOffByOne, Plant::None], false);
    assert!(outcome.is_err(), "an index one unit off went unseen");
}

#[test]
fn the_oracle_catches_a_flip_on_a_stale_base() {
    caught(16, [Plant::StaleBase, Plant::StaleBase], false, |_| None);
}

/// Two writers each commit while the other does: both commits land, and the flip lock is
/// contended (the second waits on its timer, bounded).
#[test]
fn two_writers_commit_at_once() {
    let mut sim = Sim::new(7, Faults::default());
    let mut actors = vec![Actor::new(cfg(0, 1, false), Plant::None), Actor::new(cfg(1, 1, false), Plant::None)];
    settle(&mut sim, &mut actors);
    for round in 0..8u32 {
        for (i, actor) in actors.iter_mut().enumerate() {
            let n = round * 2 + i as u32;
            actor.store.put(key(n), Kind::Query, &value(n)).unwrap();
            actor.store.commit(Vec::new(), sim.wall_ns(), &mut actor.effects).unwrap();
        }
        settle(&mut sim, &mut actors);
        for actor in &mut actors {
            assert!(matches!(actor.store.take_committed(), Some(Ok(_))));
        }
    }
    let r = reader(&mut sim, 2, 1);
    // Generation 0 is the empty store (no slot written); each commit's flip makes the next.
    assert_eq!(r.store.generation(), 16, "16 commits by two writers, each flipped once on the one before");
    for n in 0..16 {
        assert_eq!(r.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)));
    }
}

/// A segment of an earlier boot whose bytes did not survive (here: rotted after a durable
/// commit) is checked at the first open of the new boot, dropped and reported by name with its
/// count; the plant that trusts it returns a corrupt record instead.
#[test]
fn the_oracle_catches_a_segment_of_an_earlier_boot_read_unchecked() {
    let outcome = |plant: Plant| {
        let mut sim = Sim::new(3, Faults::default());
        let mut w = [Actor::new(cfg(0, 1, true), Plant::None)];
        settle(&mut sim, &mut w);
        let [mut w] = w;
        for n in 0..10 {
            w.store.put(key(n), Kind::Query, &value(n)).unwrap();
        }
        w.store.commit(Vec::new(), sim.wall_ns(), &mut w.effects).unwrap();
        let mut w = [w];
        settle(&mut sim, &mut w);
        let committed = w[0].store.take_committed().unwrap().unwrap();
        let segment = committed.segment.unwrap();
        drop(w);
        sim.crash();
        let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
        sim.rot(&file, format::HEADER as u64 + 3, &[0xff, 0x00, 0xff]);
        let mut r = [Actor::new(cfg(1, 2, false), plant)];
        settle(&mut sim, &mut r);
        let ledger = Ledger { committed: (0..10).map(|n| (n, (segment, committed.generation))).collect(), ..Ledger::default() };
        let report = r[0].store.report().clone();
        // The rot stands for bytes a power cut did not let reach the disk: a loss to report.
        (oracle(&r[0].store, &ledger, true, false), report, file)
    };
    let (honest, report, file) = outcome(Plant::None);
    assert!(honest.is_ok(), "{honest:?}");
    assert_eq!(report.lost.len(), 1, "{report:?}");
    assert_eq!((report.lost[0].file.clone(), report.lost[0].records), (file, Some(10)));
    let (planted, ..) = outcome(Plant::TrustBoot);
    assert!(planted.is_err(), "a segment of an earlier boot was read unchecked and nothing saw it");
}

/// Verify once (decision 18): a record whose bytes rot on the disk after its segment was proved
/// (here, in the boot that wrote it) is not looked at by a read, which checks nothing; the
/// explicit scrub finds it, naming its key and segment; and in the next boot the segment's one
/// full check drops it and reports it by name.
#[test]
fn a_byte_rotted_after_its_proof_is_found_by_scrub_or_the_next_boot_not_by_a_read() {
    let mut sim = Sim::new(5, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, true), Plant::None)];
    settle(&mut sim, &mut w);
    w[0].store.put(key(9), Kind::Query, &value(9)).unwrap();
    let effects = &mut w[0].effects;
    w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
    settle(&mut sim, &mut w);
    let segment = w[0].store.take_committed().unwrap().unwrap().segment.unwrap();
    let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
    sim.rot(&file, format::HEADER as u64, &[!value(9)[0]]);
    let r = reader(&mut sim, 1, 1);
    let read = r.store.get(&key(9)).unwrap().map(|it| it.bytes.to_vec());
    assert_ne!(read, Some(value(9)), "the read returned the bytes as they lie, unchecked");
    assert_eq!(r.store.checks(), super::Checks { full: 0, reads: 1, per_read: 0 });
    let found = r.store.scrub();
    assert_eq!(found.iter().map(|it| (it.key, it.pack.clone())).collect::<Vec<_>>(), vec![(key(9), file.clone())]);
    // The next boot proves the segment once, from its digest, and drops it by name.
    let next = reader(&mut sim, 2, 2);
    assert_eq!(next.store.checks().full, 1);
    assert_eq!(next.store.report().lost.iter().map(|it| (it.file.clone(), it.records)).collect::<Vec<_>>(), vec![(file, Some(1))]);
    assert_eq!(next.store.get(&key(9)).unwrap().map(|it| it.bytes.len()), None);
}

/// The control for "verify once": an open that checks a segment of an earlier boot and does not
/// record it makes the next open of the same boot check it again, and the check the oracle
/// makes (no segment fully checked in a second open of one boot) sees it.
#[test]
fn the_oracle_catches_a_full_check_made_twice_in_one_boot() {
    let second_open_checks = |plant: Plant| {
        let mut sim = Sim::new(8, Faults::default());
        let mut w = [Actor::new(cfg(0, 1, true), Plant::None)];
        settle(&mut sim, &mut w);
        for n in 0..6 {
            w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
        drop(w);
        sim.crash();
        let mut first = [Actor::new(cfg(1, 2, false), plant)];
        settle(&mut sim, &mut first);
        assert_eq!(first[0].store.checks().full, 1);
        let second = reader(&mut sim, 2, 2);
        second.store.fully_checked().len()
    };
    assert_eq!(second_open_checks(Plant::None), 0, "an honest open records its check");
    assert_eq!(second_open_checks(Plant::ForgetCheck), 1, "the planted bug went unseen");
}

/// The files of `dir`, by name, with their bytes.
fn files(sim: &mut Sim) -> BTreeMap<String, Vec<u8>> {
    let Done::Listed(listed) = ask(sim, Effect::List { dir: dir(), tag: Tag(OWN + 1) }) else { panic!("a listing") };
    listed.into_iter().filter(|(_, meta)| !meta.dir).map(|(name, _)| (name.as_str().to_owned(), sim.contents(&name))).collect()
}

fn build(seed: u64) -> (BTreeMap<String, Vec<u8>>, Sim) {
    let mut sim = Sim::new(seed, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    for round in 0..5u32 {
        for n in round * 10..round * 10 + 10 {
            w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    (files(&mut sim), sim)
}

/// The persisted round trip: two builds of the same puts write the same bytes, file for file;
/// a reopen reads every record back byte for byte.
#[test]
fn the_same_puts_write_the_same_bytes_and_read_back() {
    let (a, mut sim) = build(11);
    let (b, _) = build(11);
    assert_eq!(a, b, "two builds of one history wrote different bytes");
    assert!(a.keys().any(|it| it.ends_with("CACHEDIR.TAG")), "{:?}", a.keys());
    let r = reader(&mut sim, 1, 1);
    for n in 0..50 {
        assert_eq!(r.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "record {n}");
    }
    // Every segment is self-describing: its own check passes on its bytes as they lie.
    for (name, bytes) in &a {
        if name.contains("/seg-") {
            format::check_segment(bytes).unwrap_or_else(|why| panic!("{name}: {why}"));
        }
    }
}

/// The head cut at every length from 0 to 4160 after three commits (generation 1 in slot 1,
/// 2 in slot 0, 3 in slot 1): never refused; a whole slot 0 (64 bytes or more) reads
/// generation 2, every record of generations 1 and 2 back, and generation 3's segment reported
/// lost by name with the torn slot; shorter, no generation survives and every segment is read
/// from its footer and reported lost; the whole head reads generation 3 and loses nothing. A
/// second open reports nothing again.
#[test]
fn a_head_cut_at_any_length_reads_the_slot_before_or_the_orphans_and_says_so() {
    let mut sim = Sim::new(21, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    let mut ledger = Ledger::default();
    let mut segments = Vec::new();
    for round in 0..3u32 {
        for n in round * 4..round * 4 + 4 {
            w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        let committed = w[0].store.take_committed().unwrap().unwrap();
        assert_eq!(committed.generation, u64::from(round) + 1);
        let segment = committed.segment.unwrap();
        segments.push(segment);
        for n in round * 4..round * 4 + 4 {
            ledger.committed.insert(n, (segment, committed.generation));
        }
    }
    let saved = files(&mut sim);
    let head = saved["plain/head"].clone();
    assert_eq!(head.len(), format::HEAD_LEN);
    for cut in 0..=format::HEAD_LEN {
        let mut sim = Sim::new(22, Faults::default());
        for (name, bytes) in &saved {
            let bytes = if name == "plain/head" { head[..cut].to_vec() } else { bytes.clone() };
            let file = FileName::new(name).unwrap();
            assert!(matches!(ask(&mut sim, Effect::Write { file, bytes, tag: Tag(OWN) }), Done::Written));
        }
        let r = reader(&mut sim, 1, 2);
        let report = r.store.report().clone();
        let lost: BTreeSet<u64> = report.lost.iter().filter_map(|it| it.segment).collect();
        oracle(&r.store, &ledger, true, false).unwrap_or_else(|why| panic!("head cut to {cut}: {why}"));
        let (whole, gone): (Option<u64>, &[u64]) = match cut {
            format::HEAD_LEN => (Some(3), &[]),
            c if c >= format::SLOT => (Some(2), &segments[2..]),
            _ => (None, &segments[..]),
        };
        assert_eq!(report.whole, whole, "head cut to {cut}: {report:?}");
        for id in gone {
            assert!(lost.contains(id), "head cut to {cut}: segment {id:016x} gone and not reported: {report:?}");
        }
        if cut < format::HEAD_LEN && cut > 0 {
            assert!(report.lost.iter().any(|it| it.file.as_str() == "plain/head"), "head cut to {cut}: the torn slot not reported: {report:?}");
        }
        if cut == format::HEAD_LEN {
            assert!(report.lost.is_empty(), "the whole head lost something: {report:?}");
        }
        for (&n, &(_, generation)) in &ledger.committed {
            if whole.is_some_and(|it| generation <= it) {
                assert_eq!(r.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "head cut to {cut}: record {n}");
            }
        }
        let again = reader(&mut sim, 2, 2);
        assert!(again.store.report().lost.is_empty(), "head cut to {cut}: reported again: {:?}", again.store.report());
        assert!(again.store.fully_checked().is_empty(), "head cut to {cut}: checked twice in one boot");
    }
}

/// A file of the previous layout is refused naming it, never read or overwritten: the old
/// store's `head`, and a slot of format 0.
#[test]
fn a_previous_layout_is_refused_by_name() {
    let refused = |head: Vec<u8>| {
        let mut sim = Sim::new(1, Faults::default());
        let file = FileName::new("plain/head").unwrap();
        assert!(matches!(ask(&mut sim, Effect::Write { file, bytes: head, tag: Tag(OWN) }), Done::Written));
        let mut r = [Actor::new(cfg(0, 1, false), Plant::None)];
        settle(&mut sim, &mut r);
        r[0].store.broken().map(str::to_owned).expect("refused")
    };
    let old = refused(b"BLZHEAD\x03 a content-addressed store's head".to_vec());
    assert!(old.contains("plain/head") && old.contains("another store"), "{old}");
    let mut slot = [0u8; format::SLOT];
    slot[0..4].copy_from_slice(&format::HEAD_MAGIC);
    slot[8..16].copy_from_slice(&2u64.to_le_bytes());
    let crc = blaze_ids::crc32c::crc32c(&slot[..60]);
    slot[60..64].copy_from_slice(&crc.to_le_bytes());
    let mut head = vec![0u8; format::HEAD_LEN];
    head[..format::SLOT].copy_from_slice(&slot);
    let zero = refused(head);
    assert!(zero.contains("plain/head") && zero.contains("format 0"), "{zero}");
}

/// Compaction under a reader: past [`super::SEGMENTS_MOST`] segments the oldest are merged;
/// over the cap what no root names and is older than the window goes; a reader opened before
/// keeps reading what it read; every file a generation named stays until a grace period after
/// it was retired, and goes after it.
#[test]
fn compaction_keeps_roots_and_the_grace_period() {
    let mut sim = Sim::new(9, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    let mut named = BTreeSet::new();
    for n in 0..40u32 {
        w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        if n == 39 {
            w[0].store.root("kept", &[key(1), key(2), key(3)]).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        let segment = w[0].store.take_committed().unwrap().unwrap().segment.unwrap();
        named.insert(segment);
    }
    let before = reader(&mut sim, 1, 1);
    // A merge first (no cap): fewer segments, everything kept.
    let mut slices = 0;
    loop {
        assert!(w[0].store.compact(&mut w[0].effects));
        settle(&mut sim, &mut w);
        let sliced = w[0].store.take_sliced().unwrap();
        slices += 1;
        assert!(!sliced.aborted);
        if !sliced.more || slices > 20 {
            break;
        }
    }
    assert!(w[0].store.segments().len() <= super::SEGMENTS_MOST, "{} segments", w[0].store.segments().len());
    let fresh = reader(&mut sim, 2, 1);
    for n in 0..40 {
        assert_eq!(fresh.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "merged record {n}");
        assert_eq!(before.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "a reader of before, record {n}");
    }
    // Within the grace period every file any generation named is still there.
    for id in &named {
        assert!(sim.exists(&FileName::new(&format!("plain/seg-{id:016x}")).unwrap()), "seg {id:016x} went inside the grace period");
    }
    // Past it, the next slice unlinks what was retired.
    wait(&mut sim, Duration::from_nanos(super::GRACE_NS + 1_000_000_000));
    assert!(w[0].store.compact(&mut w[0].effects));
    settle(&mut sim, &mut w);
    let sliced = w[0].store.take_sliced().unwrap();
    assert!(sliced.removed > 0, "{sliced:?}");
    let live: BTreeSet<u64> = w[0].store.segments().iter().map(|it| it.0).collect();
    for id in named.difference(&live) {
        assert!(!sim.exists(&FileName::new(&format!("plain/seg-{id:016x}")).unwrap()), "retired seg {id:016x} stayed past the grace period");
    }
}

/// Over a cap, records no root names that are older than the window go; rooted ones stay.
#[test]
fn pruning_keeps_what_roots_name() {
    let mut sim = Sim::new(13, Faults::default());
    let mut c = cfg(0, 1, false);
    c.cap_bytes = 1;
    let mut w = [Actor { store: Plain::planted(c, Plant::None), effects: Vec::new() }];
    w[0].store.open(&mut w[0].effects);
    settle(&mut sim, &mut w);
    w[0].store.root("kept", &[key(0), key(1)]).unwrap();
    for n in 0..4u32 {
        w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
    }
    let effects = &mut w[0].effects;
    w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
    settle(&mut sim, &mut w);
    assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    // Age it past the window.
    for n in 100..100 + super::WINDOW as u32 + 1 {
        w[0].store.put(key(n), Kind::Query, &value(n)).unwrap();
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new(), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    assert!(w[0].store.compact(&mut w[0].effects));
    settle(&mut sim, &mut w);
    let sliced = w[0].store.take_sliced().unwrap();
    assert!(sliced.dropped >= 2, "{sliced:?}");
    let r = reader(&mut sim, 1, 1);
    for n in [0, 1] {
        assert_eq!(r.store.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "rooted record {n}");
    }
    for n in [2, 3] {
        assert_eq!(r.store.get(&key(n)).unwrap().map(|it| it.bytes.len()), None, "unrooted old record {n}");
    }
}

/// The interpolation search over ranged reads finds every key of a sorted run and no other, in
/// a bounded number of reads.
#[test]
fn a_ranged_index_probe_finds_every_key_in_few_reads() {
    let mut entries = Vec::new();
    for n in 0..20_000u32 {
        entries.push(format::Entry { key: key(n), kind: Kind::Query, at: format::HEADER as u64, len: 0, crc: 0 });
    }
    let run = format::sorted_run(entries).unwrap();
    let count = (run.len() / format::ENTRY) as u32;
    let mut most = 0;
    for n in (0..20_000u32).step_by(97).chain([20_001, 30_000]) {
        let mut reads = 0;
        let found = format::find_ranged(count, &key(n), |at, len| {
            reads += 1;
            Ok(run[at as usize..at as usize + len].to_vec())
        })
        .unwrap();
        assert_eq!(found.is_some(), n < 20_000, "key {n}");
        most = most.max(reads);
    }
    assert!(most <= 3, "a probe took {most} ranged reads of a 20,000-entry run");
}

/// A test's signer: a statement is the keyed hash of (namespace, digest, length) under a
/// secret only the trusted builder knows, standing in for a signature.
struct Keyed(&'static str);

impl Keyed {
    fn sign(&self, namespace: Namespace, digest: &ContentId, len: u64) -> Vec<u8> {
        let mut bytes = self.0.as_bytes().to_vec();
        bytes.extend_from_slice(&namespace.0.to_le_bytes());
        bytes.extend_from_slice(&digest.0);
        bytes.extend_from_slice(&len.to_le_bytes());
        ContentId::of("plain store test statement", &bytes).0.to_vec()
    }
}

impl Verifier for Keyed {
    fn verify(&self, namespace: Namespace, digest: &ContentId, len: u64, statement: &[u8]) -> Result<(), String> {
        (statement == self.sign(namespace, digest, len)).then_some(()).ok_or_else(|| "not signed by the trusted builder".to_owned())
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-plain-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A store over the simulation, through the driver.
fn driven(n: u64, namespace: Namespace) -> Driven<Sim> {
    let mut c = cfg(n, 1, false);
    c.namespace = namespace;
    Driven::with(Sim::new(n, Faults::default()), Path::new("/sim"), c).unwrap()
}

/// A trusted builder's segment in the public namespace, its bytes as they lie, in a source.
fn published(source: &LocalDir, signer: &Keyed) -> (Vec<u8>, ContentId) {
    let mut ci = driven(0, Namespace::PUBLIC);
    for n in 0..8 {
        ci.put(key(n), Kind::Query, &value(n)).unwrap();
    }
    let segment = ci.commit_with(Vec::new()).unwrap().segment.unwrap();
    let (seg, bytes) = ci.core().segment(segment).unwrap();
    let bytes = bytes.unwrap().to_vec();
    source.put(&bytes, &signer.sign(Namespace::PUBLIC, &seg.digest, seg.len)).unwrap();
    (bytes, seg.digest)
}

/// A pulled segment is the bytes a builder wrote, mapped as a local one; a forged one (bytes
/// changed under a real statement, or a statement nobody trusted signed) is refused before it
/// is written; and a key is never answered from a namespace the caller did not ask for.
#[test]
fn a_fetched_segment_is_verified_before_it_is_used() {
    let signer = Keyed("the trusted builder's secret");
    let at = scratch("fetch");
    let mut source = LocalDir::new(&at);
    let (bytes, digest) = published(&source, &signer);
    let keys: Vec<Key> = (0..8).map(key).collect();

    let mut me = driven(1, Namespace::LOCAL);
    assert_eq!(me.get(&key(0)).unwrap().map(|it| it.bytes.len()), None);
    let pulled = me.fetch_missing(&mut source, &signer, Namespace::PUBLIC, &keys).unwrap();
    assert_eq!(pulled.len(), 1);
    for n in 0..8 {
        assert_eq!(me.get(&key(n)).unwrap().map(|it| it.bytes.to_vec()), Some(value(n)), "pulled record {n}");
    }

    // Forged: one payload byte changed, under the real name and statement.
    let mut forged = bytes.clone();
    forged[format::HEADER] ^= 1;
    let hex: String = digest.0.iter().map(|it| format!("{it:02x}")).collect();
    std::fs::write(at.join(format!("{hex}.seg")), &forged).unwrap();
    let mut other = driven(2, Namespace::LOCAL);
    let why = other.fetch_missing(&mut source, &signer, Namespace::PUBLIC, &keys).expect_err("a forged segment is refused");
    assert!(why.contains("digest"), "{why}");
    assert_eq!(other.get(&key(0)).unwrap().map(|it| it.bytes.len()), None, "nothing of it was used");

    // A whole, consistent segment no trusted builder signed.
    std::fs::write(at.join(format!("{hex}.seg")), &bytes).unwrap();
    let why = other.fetch_missing(&mut source, &Keyed("someone else"), Namespace::PUBLIC, &keys).expect_err("an unsigned segment is refused");
    assert!(why.contains("not trusted"), "{why}");

    // A namespace not asked for is never answered from.
    let org = Namespace::named("an organisation");
    let why = other.fetch_missing(&mut source, &signer, org, &keys).expect_err("a namespace the store does not read");
    assert!(why.contains("not one this store reads"), "{why}");
    assert!(source.find(Namespace::LOCAL, &keys).unwrap().is_empty(), "a public segment answered for another namespace");

    // A source that lies about the namespace is refused.
    struct Liar(LocalDir);
    impl Source for Liar {
        fn find(&mut self, _: Namespace, keys: &[Key]) -> Result<Vec<Found>, String> {
            let mut found = self.0.find(Namespace::PUBLIC, keys)?;
            for it in &mut found {
                it.segment.namespace = Namespace::LOCAL;
            }
            Ok(found)
        }
        fn fetch(&mut self, digest: &ContentId, at: u64, len: u64) -> Result<Vec<u8>, String> {
            self.0.fetch(digest, at, len)
        }
        fn statement(&mut self, digest: &ContentId) -> Result<Vec<u8>, String> {
            self.0.statement(digest)
        }
    }
    // The statement binds the namespace, so the verifier refuses the lie before a byte is
    // fetched.
    let mut liar = Liar(LocalDir::new(&at));
    let why = other.fetch_missing(&mut liar, &signer, Namespace::LOCAL, &keys).expect_err("a segment of another namespace than claimed");
    assert!(why.contains("not trusted"), "{why}");
    // With a verifier that trusts every statement (one that is wrong or compromised), the
    // namespace written in the segment's own bytes still refuses it, and nothing of it is used.
    struct TrustsAll;
    impl Verifier for TrustsAll {
        fn verify(&self, _: Namespace, _: &ContentId, _: u64, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
    }
    let why = other.fetch_missing(&mut liar, &TrustsAll, Namespace::LOCAL, &keys).expect_err("a segment whose bytes say another namespace");
    assert!(why.contains("not the one named and verified"), "{why}");
    assert_eq!(other.get(&key(0)).unwrap().map(|it| it.bytes.len()), None, "nothing of the lie was used");
    let _ = std::fs::remove_dir_all(&at);
}
