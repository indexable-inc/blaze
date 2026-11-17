//! The command's store on the plain store, on blaze-core's simulation (rules 43 and 64): each
//! property has an oracle and a planted bug it catches. The plain store's own durability (torn
//! segments, torn slots, the flip, compaction's grace) is proven by blaze-cas's simulation
//! (`crates/blaze-cas/src/plain/tests.rs`); these prove what this module adds: bytes lent in
//! place stay what they were, puts reach the disk whole by their commit, two values are
//! refused, a pin keeps a running program's records through compaction, the old home is
//! reported once and never touched.

use std::alloc::Global;

use blaze_cas::plain::Settings;
use blaze_core::sim::{Faults, Sim};
use blaze_core::{Done, Effect, FileName, Tag as EffectTag};
use blaze_drive::Key;
use blaze_ids::ContentId;
use blaze_keyed::kind::Namespace;

use super::{Facts, Maps, OWN_TAGS, PlaceError, PlacePlant, PlainPlace, drive_alone};

fn key(n: u32) -> Key {
    Key(ContentId::of("plain place test key", &n.to_le_bytes()))
}

/// Record `n`'s bytes: lengths 0 to 299, so some end inside a block and one is empty.
fn value(n: u32) -> Vec<u8> {
    (0..(n as usize * 37) % 300).map(|i| (n as usize).wrapping_mul(131).wrapping_add(i) as u8).collect()
}

fn facts(boot: u64) -> Facts {
    Facts { home: None, boot: Some(boot), nonce: 1000 + boot }
}

fn open(sim: Sim, settings: &Settings, boot: u64, plant: PlacePlant) -> Result<Maps<'static, Sim>, PlaceError> {
    Maps::open_planted(sim, settings, facts(boot), blaze_own::testing::heap().alloc(), plant)
}

fn aligned(bytes: &[u8]) -> bool {
    bytes.as_ptr() as usize % 16 == 0
}

/// Bytes lent by a get stay equal to the value through later puts (a new arena chunk among
/// them), a commit and a compaction; every lend starts 16-aligned; after a reopen the same keys
/// are answered from the segments, equal and aligned.
/// Expected: `test store::plain_place::tests::lent_bytes_stay_through_puts_commits_and_a_reopen ... ok`.
#[test]
fn lent_bytes_stay_through_puts_commits_and_a_reopen() {
    let settings = Settings::default();
    let maps = open(Sim::new(1, Faults::default()), &settings, 1, PlacePlant::None).expect("open");
    let mut lent = Vec::new_in(Global);
    for n in 0..200u32 {
        maps.put(key(n), Namespace::LOCAL, &value(n)).expect("put");
        let held = maps.get(key(n)).expect("its own put");
        assert!(aligned(held.bytes) || held.bytes.is_empty(), "record {n} lent unaligned");
        lent.push((n, held.bytes));
    }
    // A record past one chunk: a chunk of its own.
    let big: Vec<u8> = (0..(1 << 21)).map(|i| i as u8).collect();
    maps.put(key(10_000), Namespace::LOCAL, &big).expect("a big put");
    maps.commit().expect("commit");
    for n in 200..260u32 {
        maps.put(key(n), Namespace::LOCAL, &value(n)).expect("put after a commit");
    }
    maps.after_answer(4).expect("after the answer");
    for (n, bytes) in &lent {
        assert_eq!(*bytes, &value(*n)[..], "record {n}: lent bytes changed under their reader");
    }
    assert_eq!(maps.get(key(10_000)).expect("big").bytes, &big[..]);
    let sim = maps.into_driver();
    let again = open(sim, &settings, 1, PlacePlant::None).expect("reopen");
    for n in 0..260u32 {
        let held = again.get(key(n)).unwrap_or_else(|| panic!("record {n} committed and not found"));
        assert_eq!(held.bytes, &value(n)[..], "record {n} read back");
        assert!(aligned(held.bytes) || held.bytes.is_empty(), "record {n} unaligned in its segment");
    }
    let counts = again.counts();
    assert_eq!(counts.from_puts, 0, "a fresh process holds no puts");
    assert_eq!(counts.from_segments, 260);
}

/// The oracle of "a commit that returned Ok wrote every put before it": durable settings, a
/// crash after the commit, every key read back. `Err` names the first key missing.
fn every_committed_put_survives(plant: PlacePlant, seed: u64) -> Result<(), String> {
    let settings = Settings { durable: true, ..Settings::default() };
    let maps = open(Sim::new(seed, Faults::default()), &settings, 1, plant).map_err(|e| format!("open: {e}"))?;
    for n in 0..64u32 {
        maps.put(key(n), Namespace::LOCAL, &value(n)).map_err(|e| format!("put: {e}"))?;
    }
    maps.commit().map_err(|e| format!("commit: {e}"))?;
    let mut sim = maps.into_driver();
    sim.crash();
    let again = open(sim, &settings, 2, PlacePlant::None).map_err(|e| format!("reopen: {e}"))?;
    for n in 0..64u32 {
        match again.get(key(n)) {
            Some(held) if held.bytes == &value(n)[..] => {}
            Some(_) => return Err(format!("record {n} read back with other bytes")),
            None => return Err(format!("record {n} lost by a commit that returned Ok")),
        }
    }
    Ok(())
}

/// Expected: `test store::plain_place::tests::a_durable_commit_keeps_every_put ... ok`.
#[test]
fn a_durable_commit_keeps_every_put() {
    for seed in 0..16 {
        every_committed_put_survives(PlacePlant::None, seed).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
    }
}

/// Expected: `test store::plain_place::tests::the_oracle_catches_a_commit_that_drops_earlier_puts ... ok`.
#[test]
fn the_oracle_catches_a_commit_that_drops_earlier_puts() {
    let why = every_committed_put_survives(PlacePlant::DropsEarlierPuts, 3).expect_err("the planted bug is caught");
    assert!(why.contains("lost by a commit"), "{why}");
}

/// A power cut at a drawn point of a command (the default mode: nothing synced), the cut tearing
/// what was not written back as the simulation draws it: after the next boot's open, every
/// record is whole or absent, never other bytes (the flip is the commit; an unsealed segment of
/// an earlier boot is checked before its bytes are trusted). Effects failing inside one commit
/// are blaze-cas's own simulation's (`plain/tests.rs`); here the cut falls between the
/// command's steps, which is all this layer adds.
/// Expected: `test store::plain_place::tests::a_power_cut_anywhere_leaves_each_record_whole_or_absent ... ok`.
#[test]
fn a_power_cut_anywhere_leaves_each_record_whole_or_absent() {
    let settings = Settings::default();
    for seed in 0..64u64 {
        // The step after which the power goes: 0 to 44 (40 puts, two commits, the tail).
        let cut_after = (seed * 7) % 45;
        let maps = open(Sim::new(seed, Faults::default()), &settings, 1, PlacePlant::None).expect("open");
        let mut step = 0u64;
        'command: {
            for n in 0..40u32 {
                maps.put(key(n), Namespace::LOCAL, &value(n)).expect("put");
                step += 1;
                if step > cut_after {
                    break 'command;
                }
                if n == 13 || n == 29 {
                    maps.commit().expect("commit");
                    step += 1;
                    if step > cut_after {
                        break 'command;
                    }
                }
            }
            maps.after_answer(2).expect("after the answer");
        }
        let mut sim = maps.into_driver();
        sim.crash();
        let reader = open(sim, &settings, 2, PlacePlant::None).unwrap_or_else(|e| panic!("seed {seed}: the next boot's open: {e}"));
        for n in 0..40u32 {
            if let Some(held) = reader.get(key(n)) {
                assert_eq!(held.bytes, &value(n)[..], "seed {seed}: record {n} read back torn");
            }
        }
    }
}

/// The oracle of rule 21 at the store: other bytes under a committed key are refused by name.
fn other_bytes_refused(plant: PlacePlant) -> Result<(), String> {
    let settings = Settings::default();
    let maps = open(Sim::new(4, Faults::default()), &settings, 1, PlacePlant::None).map_err(|e| format!("open: {e}"))?;
    maps.put(key(1), Namespace::LOCAL, b"one value").map_err(|e| format!("put: {e}"))?;
    // Equal bytes again: left as they are.
    maps.put(key(1), Namespace::LOCAL, b"one value").map_err(|e| format!("an equal put: {e}"))?;
    maps.commit().map_err(|e| format!("commit: {e}"))?;
    let again = open(maps.into_driver(), &settings, 1, plant).map_err(|e| format!("reopen: {e}"))?;
    match again.put(key(1), Namespace::LOCAL, b"another value") {
        Err(PlaceError::TwoValues { key: k }) if k == key(1) => Ok(()),
        Err(other) => Err(format!("refused for another reason: {other}")),
        Ok(()) => Err("two values under one key were taken".to_owned()),
    }
}

/// Expected: `test store::plain_place::tests::two_values_under_one_key_are_refused ... ok`.
#[test]
fn two_values_under_one_key_are_refused() {
    other_bytes_refused(PlacePlant::None).expect("refused");
    let why = other_bytes_refused(PlacePlant::SkipsStoreCheck).expect_err("the planted bug is caught");
    assert!(why.contains("two values"), "{why}");
}

/// The oracle of a running program across compaction (drive.md ruling 6; blaze-cas plain.rs,
/// "Compaction"): a record lent from a segment stays equal through a compaction in the same
/// process (its mapping stands; on a real disk the file may be unlinked a grace period later and
/// the mapping still stands), and a record its program pinned is found by the next process after
/// compaction pruned past the cap.
fn pinned_through_compaction(plant: PlacePlant) -> Result<(), String> {
    // A cap of 0: compaction prunes every record no root names, older than its window.
    let settings = Settings { cap_mib: Some(0), ..Settings::default() };
    let mut sim = Sim::new(6, Faults::default());
    // Twenty-four commands, one record each: twenty-four generations, past the window.
    for n in 0..24u32 {
        let maps = open(sim, &settings, 1, PlacePlant::None).map_err(|e| format!("open {n}: {e}"))?;
        maps.put(key(n), Namespace::LOCAL, &value(n + 1)).map_err(|e| format!("put {n}: {e}"))?;
        maps.commit().map_err(|e| format!("commit {n}: {e}"))?;
        sim = maps.into_driver();
    }
    // The command that starts the program: its records pinned, one lent from its segment.
    let maps = open(sim, &settings, 1, plant).map_err(|e| format!("open: {e}"))?;
    let lent = maps.get(key(0)).ok_or("record 0 not found before compaction")?.bytes;
    maps.pin("program 0", &[key(0)]).map_err(|e| format!("pin: {e}"))?;
    maps.after_answer(64).map_err(|e| format!("after the answer: {e}"))?;
    if lent != &value(1)[..] {
        return Err("bytes lent from a segment changed through a compaction".to_owned());
    }
    let next = open(maps.into_driver(), &settings, 1, PlacePlant::None).map_err(|e| format!("reopen: {e}"))?;
    match next.get(key(0)) {
        Some(held) if held.bytes == &value(1)[..] => Ok(()),
        Some(_) => Err("the pinned record read back with other bytes".to_owned()),
        None => Err("the pinned record was let go by compaction".to_owned()),
    }
}

/// Expected: `test store::plain_place::tests::a_pinned_program_keeps_its_records_through_compaction ... ok`.
#[test]
fn a_pinned_program_keeps_its_records_through_compaction() {
    pinned_through_compaction(PlacePlant::None).expect("kept");
    let why = pinned_through_compaction(PlacePlant::ForgetsPin).expect_err("the planted bug is caught");
    assert!(why.contains("let go by compaction"), "{why}");
}

/// The old home (rule 26): the stand-in's folder is reported by name once, never read (its
/// records are made again), never deleted; the next command does not look again.
/// Expected: `test store::plain_place::tests::an_old_home_is_reported_once_and_left_alone ... ok`.
#[test]
fn an_old_home_is_reported_once_and_left_alone() {
    let settings = Settings::default();
    let mut sim = Sim::new(8, Faults::default());
    let old = FileName::new("v2/own/pack-0000000000000001").expect("a name");
    let mut effects = Vec::with_capacity_in(1, Global);
    effects.push(Effect::WriteUnsynced { file: old.clone(), bytes: b"BLZP an old pack".to_vec(), tag: EffectTag(OWN_TAGS) });
    let done = drive_alone(&mut sim, effects, &["the old pack"], core::time::Duration::from_secs(10)).expect("written");
    assert!(!matches!(done[0], Done::Failed(_)), "the old pack written");
    let maps = open(sim, &settings, 1, PlacePlant::None).expect("open");
    let found: Vec<&str> = maps.old_home().collect();
    assert_eq!(found, ["v2/own"], "the old folder by name");
    assert_eq!(maps.old_home().count(), 0, "reported once");
    assert!(maps.get(key(1)).is_none(), "nothing of the old home is read");
    maps.after_answer(0).expect("the mark committed");
    let mut sim = maps.into_driver();
    let mut effects = Vec::with_capacity_in(1, Global);
    effects.push(Effect::Stat { file: old, tag: EffectTag(OWN_TAGS) });
    let done = drive_alone(&mut sim, effects, &["the old pack"], core::time::Duration::from_secs(10)).expect("looked");
    assert!(matches!(done[0], Done::Stat(Some(_))), "the old pack is never deleted");
    let next = open(sim, &settings, 1, PlacePlant::None).expect("reopen");
    assert_eq!(next.old_home().count(), 0, "a later command does not look again");
}

/// The namespace (drive.md ruling 2): `PUBLIC` only for a record whose names are all public on
/// a trusted builder; a developer's machine writes everything `LOCAL`.
/// Expected: `test store::plain_place::tests::a_developer_machine_writes_everything_local ... ok`.
#[test]
fn a_developer_machine_writes_everything_local() {
    let mine = open(Sim::new(9, Faults::default()), &Settings::default(), 1, PlacePlant::None).expect("open");
    let place = PlainPlace::new(&mine);
    assert_eq!(place.namespace(true), Namespace::LOCAL);
    assert_eq!(place.namespace(false), Namespace::LOCAL);
    assert!(matches!(mine.put(key(1), Namespace::PUBLIC, b"x"), Err(PlaceError::NotWritable { .. })));
    let ci = open(Sim::new(10, Faults::default()), &Settings { trusted_writer: true, ..Settings::default() }, 1, PlacePlant::None).expect("open");
    let place = PlainPlace::new(&ci);
    assert_eq!(place.namespace(true), Namespace::PUBLIC);
    assert_eq!(place.namespace(false), Namespace::LOCAL);
}
