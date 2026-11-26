//! The expansion records under power cuts and failures (rule 43: proven by a simulation, never
//! by review). Oracle: after any crash every record read is one that was written, whole. The
//! planted bug, a read that takes a file without its seal, must be caught.

use blaze_ids::ContentId;
use blaze_record::{Kind, Store};
use bstd::cx::Cx;
use bstd::io::Io;
use bstd::test::{Given, Seeds, seeded};

use super::{DirStore, FilesError, OnDir, RecordPlant};

const KEYS: u32 = 24;

fn key(k: u32) -> ContentId {
    ContentId::of("blaze-proc-macro record test", &k.to_le_bytes())
}

/// The longest value: past two sectors, so a cut can tear one.
const LONGEST: usize = 1400;

/// Key `k`'s one value (rule 21), written into `into`: from empty to past two sectors.
fn payload(k: u32, into: &mut [u8; LONGEST]) -> &[u8] {
    let len = (k as usize * 131) % LONGEST;
    for (i, byte) in into[..len].iter_mut().enumerate() {
        *byte = (k as usize * 7 + i * 13 + 1) as u8;
    }
    &into[..len]
}

/// Every key: a miss, or its whole value. A finding panics: it fails the seed (an error is a
/// restart, as a failed command is).
fn oracle(store: &DirStore) -> Result<(), FilesError> {
    let mut want = [0; LONGEST];
    for k in 0..KEYS {
        let want = payload(k, &mut want);
        if let Some(bytes) = store.get(Kind::List, &key(k))? {
            assert!(bytes == want, "record oracle: key {k} read back as other bytes ({} for {})", bytes.len(), want.len());
        }
    }
    Ok(())
}

/// One start: the oracle on what the folder holds, every put, the oracle again.
fn program(io: &Io, plant: RecordPlant) -> Result<(), FilesError> {
    let dir = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
    oracle(&store)?;
    let mut value = [0; LONGEST];
    for k in 0..KEYS {
        store.put(Kind::List, key(k), payload(k, &mut value))?;
    }
    oracle(&store)
}

fn crashes(count: u32, plant: RecordPlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
    seeded("blaze-proc-macro", module_path!(), "records_crash", Seeds::DEFAULT.from(0, count), Given::NOTHING, |_: &Cx, io: Io| {
        program(&io, plant)
    })
}

/// The invariant: after any crash or I/O failure during proc-macro expansion persistence, every record read back is whole and sealed or absent.
#[test]
fn a_crash_or_failure_anywhere_keeps_each_record_whole_or_absent() {
    match crashes(1000, RecordPlant::None) {
        Ok(passed) => {
            // Proof the faults ran (rule 61).
            std::println!("records under crashes: {:?}", passed.counts);
            assert!(passed.counts.cuts > 500, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn the_oracle_catches_a_read_that_skips_the_seal() {
    match crashes(1000, RecordPlant::SealSkipped) {
        Ok(passed) => panic!("the planted SealSkipped was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            std::println!("planted SealSkipped caught: {}", said.lines().next().unwrap_or(""));
            assert!(said.contains("record oracle"), "the planted SealSkipped failed otherwise: {said}");
        }
    }
}

/// Durable-state DST for `DirStore::{get_c_probe, put_c_probe}`: verifies crash/failure
/// whole-or-absent invariant, atomic compare-and-publish conflict detection (`OtherBytes`),
/// and oversized-record replacement across seeded fault schedules.
fn c_probe_program(io: &Io, plant: RecordPlant) -> Result<(), FilesError> {
    let dir = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
    let alloc = blaze_own::testing::heap().alloc();
    let check_oracle = |store: &DirStore| -> Result<(), FilesError> {
        let mut want = [0u8; LONGEST];
        for k in 0..8u32 {
            let want = payload(k, &mut want);
            if let Some(bytes) = store.get_c_probe(&key(k), alloc)? {
                assert!(
                    bytes.as_slice() == want,
                    "c-probe oracle: key {k} read back as other bytes ({} for {})",
                    bytes.len(),
                    want.len()
                );
            }
        }
        Ok(())
    };
    check_oracle(&store)?;
    let mut value = [0u8; LONGEST];
    for k in 0..8u32 {
        let want = payload(k, &mut value);
        store.put_c_probe(&key(k), want)?;
        // Equal re-put must succeed idempotently.
        store.put_c_probe(&key(k), want)?;
        // Conflicting second writer under the same key MUST fail with OtherBytes and NEVER
        // silently replace the committed record.
        let mut conflict = std::vec::Vec::from(want);
        conflict.push(0xA5);
        match store.put_c_probe(&key(k), &conflict) {
            Err(FilesError::OtherBytes { .. }) => {}
            Ok(()) => assert!(false, "c-probe oracle: conflicting put_c_probe on key {k} silently succeeded"),
            Err(other) => return Err(other),
        }
    }
    check_oracle(&store)
}

#[test]
fn c_probe_compare_and_publish_survives_crashes_and_rejects_conflicts() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_cas_crash",
        Seeds::DEFAULT.from(0, 300),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_program(&io, RecordPlant::None),
    ) {
        Ok(passed) => {
            assert!(passed.counts.cuts > 100, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 100, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn c_probe_dst_catches_planted_blind_overwrite() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_cas_planted_bug",
        Seeds::DEFAULT.from(0, 32),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_program(&io, RecordPlant::BlindOverwrite),
    ) {
        Ok(passed) => panic!("planted BlindOverwrite was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            assert!(said.contains("c-probe oracle"), "planted BlindOverwrite failed otherwise: {said}");
        }
    }
}

/// Concurrent corrupted-record repair interleaving: places a corrupted file at `c-probe/<k>`,
/// races two workers repairing it with conflicting answers (`want_a` vs `want_b`) on a shared
/// store, and asserts exact-one-winner + `OtherBytes` on the loser + winner intact on disk.
fn run_concurrent_repair_trial(plant: RecordPlant) -> Result<(), &'static str> {
    let dir_path = std::env::temp_dir().join(std::format!(
        "blaze-cprobe-repair-{}-{:?}",
        std::process::id(),
        plant
    ));
    let _ = std::fs::remove_dir_all(&dir_path);
    std::fs::create_dir_all(&dir_path).map_err(|_| "create repair temp dir")?;
    let (io, root) = super::native_world(&dir_path).map_err(|_| "repair native world")?;
    let alloc = blaze_own::testing::heap().alloc();
    let cx = io.cx(core::time::Duration::from_secs(10), bstd::mem::TASK_FRAME).map_err(|_| "repair cx")?;
    let k = key(99);
    let name = DirStore::named(b"c-probe/", &k);
    let rel = bstd::path::Path::new(name.as_str()).map_err(|_| "rel")?;
    let want_a = b"answer-A-from-worker-1";
    let want_b = b"answer-B-from-worker-2-diff";

    for _ in 0..8u32 {
        // Seed a torn/corrupted record at `c-probe/<k>` so both racing writers lose
        // `publish_if_absent` and enter the repair path.
        root.publish(&cx, &rel, bstd::fs::Content::bytes(b"corrupt-short"), bstd::fs::Durable::No)
            .map_err(|_| "seed corrupt")?;
        let d1 = io.need_fs().map_err(|_| "fs1")?.root();
        let d2 = io.need_fs().map_err(|_| "fs2")?.root();
        let d3 = io.need_fs().map_err(|_| "fs3")?.root();
        let s1 = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir: d1 }, plant);
        let s2 = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir: d2 }, plant);
        let (r1, r2) = std::thread::scope(|s| {
            let t1 = s.spawn(|| s1.put_c_probe(&k, want_a));
            let t2 = s.spawn(|| s2.put_c_probe(&k, want_b));
            (t1.join(), t2.join())
        });
        let (Ok(r1), Ok(r2)) = (r1, r2) else {
            let _ = std::fs::remove_dir_all(&dir_path);
            return Err("repair thread panicked");
        };
        let held = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir: d3 }, RecordPlant::None)
            .get_c_probe(&k, alloc)
            .map_err(|_| "read after repair")?
            .ok_or("record missing after repair")?;
        match (r1, r2) {
            (Ok(()), Err(FilesError::OtherBytes { .. })) => {
                if held.as_slice() != want_a {
                    let _ = std::fs::remove_dir_all(&dir_path);
                    return Err("winner A clobbered");
                }
            }
            (Err(FilesError::OtherBytes { .. }), Ok(())) => {
                if held.as_slice() != want_b {
                    let _ = std::fs::remove_dir_all(&dir_path);
                    return Err("winner B clobbered");
                }
            }
            (Ok(()), Ok(())) => {
                let _ = std::fs::remove_dir_all(&dir_path);
                return Err("silent overwrite: both conflicting repairers succeeded");
            }
            _ => {
                let _ = std::fs::remove_dir_all(&dir_path);
                return Err("unexpected repair failure");
            }
        }
    }
    // Deterministic interleaving check: Worker 1 commits valid A; Worker 2 (having observed
    // corrupt before Worker 1 committed) enters `repair_c_probe_after_invalid` directly.
    root.publish(&cx, &rel, bstd::fs::Content::bytes(b"corrupt-short"), bstd::fs::Durable::No)
        .map_err(|_| "seed corrupt 2")?;
    let d4 = io.need_fs().map_err(|_| "fs4")?.root();
    let s_clean = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir: d4 }, plant);
    s_clean.put_c_probe(&k, want_a).map_err(|_| "first repair")?;
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Other).map_err(|_| "frame")?;
    let erased = blaze_own::Erased::new(&frame);
    let f_alloc = erased.handle();
    let repair_res = s_clean.repair_c_probe_after_invalid(&name, want_b, f_alloc);
    let _ = std::fs::remove_dir_all(&dir_path);
    match repair_res {
        Err(FilesError::OtherBytes { .. }) => Ok(()),
        Ok(()) => Err("silent overwrite on interleaved repair after valid commit"),
        Err(_) => Err("unexpected error on interleaved repair"),
    }
}

#[test]
fn c_probe_concurrent_repair_serializes_and_rejects_conflicts() {
    run_concurrent_repair_trial(RecordPlant::None).expect("serialized repair must reject conflict");
}

#[test]
fn c_probe_concurrent_repair_catches_unserialized_repair() {
    let err = run_concurrent_repair_trial(RecordPlant::UnserializedRepair)
        .expect_err("planted UnserializedRepair must be caught");
    assert!(err.contains("silent overwrite"), "unexpected error: {err}");
}

/// Simulated crash/failure DST for the corrupted-record repair-locking path: seeds torn/corrupted
/// records under `bstd::test::seeded`, drives `put_c_probe` repair (`Effect::Lock` -> protected
/// bounded re-read -> `publish` -> `Effect::Unlock`) across power cuts and I/O failures, and
/// exercises the interleaved conflicting repairer after a committed repair.
fn c_probe_repair_sim_program(io: &Io, plant: RecordPlant) -> Result<(), FilesError> {
    let dir = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
    let alloc = blaze_own::testing::heap().alloc();
    let cx = io.cx(core::time::Duration::from_secs(30), bstd::mem::TASK_FRAME).map_err(|_| {
        FilesError::Stopped { name: DirStore::named(b"c-probe/", &key(0)) }
    })?;
    let fs_root = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let mut value = [0u8; LONGEST];

    // Check invariant on any state surviving a prior power cut.
    for k in 0..4u32 {
        let want = payload(k, &mut value);
        if let Some(held) = store.get_c_probe(&key(k), alloc)? {
            assert!(
                held.as_slice() == want,
                "c-probe repair oracle: key {k} survived crash with unexpected bytes ({} for {})",
                held.len(),
                want.len()
            );
        }
    }

    for k in 0..4u32 {
        let want = payload(k, &mut value);
        let name = DirStore::named(b"c-probe/", &key(k));
        let rel = bstd::path::Path::new(name.as_str()).map_err(|_| FilesError::Name { name })?;
        // If absent, seed a torn/corrupted record so `put_c_probe` enters the repair-lock path
        // (`Effect::Lock` -> bounded re-read -> `publish` -> `Effect::Unlock`) under fault injection.
        if store.get_c_probe(&key(k), alloc)?.is_none() {
            fs_root
                .publish(&cx, &rel, bstd::fs::Content::bytes(b"torn-corrupt"), bstd::fs::Durable::No)
                .map_err(FilesError::Effect)?;
        }
        store.put_c_probe(&key(k), want)?;
        // Interleaved second repairer that observed the corrupted file prior to the first writer's
        // commit MUST be rejected with `OtherBytes` once it enters the locked repair check.
        let mut conflict = std::vec::Vec::from(want);
        conflict.push(0x5A);
        match store.repair_c_probe_after_invalid(&name, &conflict, alloc) {
            Err(FilesError::OtherBytes { .. }) => {}
            Ok(()) => assert!(
                false,
                "c-probe repair oracle: conflicting interleaved repair on key {k} silently succeeded"
            ),
            Err(other) => return Err(other),
        }
        if let Some(held) = store.get_c_probe(&key(k), alloc)? {
            assert!(
                held.as_slice() == want,
                "c-probe repair oracle: key {k} corrupted after interleaved conflict"
            );
        }
    }
    Ok(())
}

#[test]
fn c_probe_repair_lock_survives_crashes_and_failures_in_sim() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_repair_lock_crash",
        Seeds::DEFAULT.from(0, 300),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_repair_sim_program(&io, RecordPlant::None),
    ) {
        Ok(passed) => {
            assert!(passed.counts.cuts > 100, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 100, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn c_probe_repair_lock_sim_catches_unserialized_repair() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_repair_lock_planted_bug",
        Seeds::DEFAULT.from(0, 32),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_repair_sim_program(&io, RecordPlant::UnserializedRepair),
    ) {
        Ok(passed) => panic!("planted UnserializedRepair was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            assert!(said.contains("c-probe repair oracle"), "planted UnserializedRepair failed otherwise: {said}");
        }
    }
}

/// Deterministic short-read simulation for `DirStore::{get_c_probe, put_c_probe}` using
/// `Sim::short_reads`: forces repeated short positional reads (`one_in = 1`) on `c-probe/` and
/// verifies that a valid sealed record is read whole without triggering false corruption repair,
/// while a conflicting `put_c_probe` is rejected with `OtherBytes`.
fn run_c_probe_short_reads_dst(plant: RecordPlant) -> Result<(), &'static str> {
    let alloc = blaze_own::testing::heap().alloc();
    for seed in 0..32u32 {
        let io = Io::sim(u64::from(seed));
        let dir = io.need_fs().map_err(|_| "no fs")?.root();
        let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
        let k = key(seed + 100);
        let want_a = b"canonical-c-probe-answer-payload-long-enough-to-split-under-short-reads-1234567890";
        let want_b = b"conflicting-c-probe-answer-that-must-never-overwrite-a-valid-sealed-record-0987654";
        store.put_c_probe(&k, want_a).map_err(|_| "initial put")?;
        let c_probe_dir = blaze_core::FileName::new("c-probe").ok_or("c-probe dir")?;
        let installed = io.with_sim(|sim| sim.short_reads(c_probe_dir, 1));
        if installed.is_none() {
            return Err("expected simulated world");
        }
        let got = store.get_c_probe(&k, alloc).map_err(|_| "get under short_reads")?;
        if got.as_deref() != Some(want_a.as_slice()) {
            return Err("c-probe short-read oracle: truncated valid sealed record");
        }
        match store.put_c_probe(&k, want_b) {
            Err(FilesError::OtherBytes { .. }) => {}
            Ok(()) => return Err("c-probe short-read oracle: falsely repaired and overwrote valid record"),
            Err(_) => return Err("unexpected error"),
        }
        let verify = store.get_c_probe(&k, alloc).map_err(|_| "verify")?;
        if verify.as_deref() != Some(want_a.as_slice()) {
            return Err("c-probe short-read oracle: lost original record");
        }
    }
    Ok(())
}

#[test]
fn c_probe_short_reads_dst_preserves_valid_records_without_repair() {
    run_c_probe_short_reads_dst(RecordPlant::None).expect("short-read loop must preserve valid records");
}

#[test]
fn c_probe_short_reads_dst_catches_planted_single_read() {
    let err = run_c_probe_short_reads_dst(RecordPlant::SingleReadCProbe)
        .expect_err("planted SingleReadCProbe must be caught under Sim::short_reads");
    assert!(err.contains("c-probe short-read oracle"), "unexpected failure: {err}");
}

/// Cross-world (multi-process) concurrency test for `publish_if_absent`: creates two completely
/// independent `native_world` instances (each starting its own `Io::fresh()` sequence at `0`)
/// sharing the same on-disk directory, synchronizes them on a barrier, and races conflicting
/// `put_c_probe` writes on the same missing key across 32 rounds. Proves sibling temporary
/// filenames include a per-world nonce so independent processes never collide on `.<key>.0.bstd-new`
/// or corrupt the winning sealed record.
fn run_cross_world_publish_race() -> Result<(), &'static str> {
    let dir_path = std::env::temp_dir().join(std::format!("blaze-cprobe-cross-world-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_path);
    std::fs::create_dir_all(&dir_path).map_err(|_| "create cross-world dir")?;
    let alloc = blaze_own::testing::heap().alloc();
    let want_a = b"cross-process-answer-A-from-independent-world-1-0123456789abcdef";
    let want_b = b"cross-process-answer-B-from-independent-world-2-fedcba9876543210-extra";

    for round in 0..32u32 {
        let k = key(500 + round);
        let (io_a, root_a) = super::native_world(&dir_path).map_err(|_| "native world A")?;
        let (io_b, root_b) = super::native_world(&dir_path).map_err(|_| "native world B")?;
        let s_a = DirStore::on(OnDir { cx: io_a.spawner(), boot: io_a.boot().ok(), dir: root_a }, RecordPlant::None);
        let s_b = DirStore::on(OnDir { cx: io_b.spawner(), boot: io_b.boot().ok(), dir: root_b }, RecordPlant::None);
        let barrier = std::sync::Barrier::new(2);
        let (ra, rb) = std::thread::scope(|scope| {
            let ta = scope.spawn(|| {
                barrier.wait();
                s_a.put_c_probe(&k, want_a)
            });
            let tb = scope.spawn(|| {
                barrier.wait();
                s_b.put_c_probe(&k, want_b)
            });
            (ta.join(), tb.join())
        });
        let (Ok(ra), Ok(rb)) = (ra, rb) else {
            let _ = std::fs::remove_dir_all(&dir_path);
            return Err("cross-world thread panicked");
        };
        let verify_dir = io_a.need_fs().map_err(|_| "verify fs")?.root();
        let held = DirStore::on(OnDir { cx: io_a.spawner(), boot: io_a.boot().ok(), dir: verify_dir }, RecordPlant::None)
            .get_c_probe(&k, alloc)
            .map_err(|_| "read after cross-world race")?
            .ok_or("record missing after cross-world race")?;
        match (ra, rb) {
            (Ok(()), Err(FilesError::OtherBytes { .. })) => {
                if held.as_slice() != want_a {
                    let _ = std::fs::remove_dir_all(&dir_path);
                    return Err("cross-world winner A corrupted by racing world B temp file");
                }
            }
            (Err(FilesError::OtherBytes { .. }), Ok(())) => {
                if held.as_slice() != want_b {
                    let _ = std::fs::remove_dir_all(&dir_path);
                    return Err("cross-world winner B corrupted by racing world A temp file");
                }
            }
            (Ok(()), Ok(())) => {
                let _ = std::fs::remove_dir_all(&dir_path);
                return Err("cross-world race allowed both conflicting puts to succeed");
            }
            _ => {
                let _ = std::fs::remove_dir_all(&dir_path);
                return Err("unexpected error in cross-world publish race");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir_path);
    Ok(())
}

#[test]
fn c_probe_cross_world_publish_if_absent_isolates_temp_files() {
    run_cross_world_publish_race().expect("independent native worlds must never collide on sibling temp files");
}

/// Deterministic simulation (DST) for cross-world `publish_if_absent` under `bstd::test::seeded`:
/// models two independent OS processes (`peer_a`, `peer_b`, both starting `Io::fresh()` at `0`)
/// sharing the same simulated disk, deterministically interleaves their `stage_publish_if_absent`
/// and `finish_publish_if_absent` phases in BOTH orders (`A-then-B` and `B-then-A`) with
/// `Durable::Yes` across seeded power cuts and sync/I/O failures, and verifies the invariant
/// oracle: every committed record is whole (`want_a` or `want_b`), never torn, and whenever a
/// peer's `finish_publish_if_absent` reports `Ok(true)`, the other peer cannot also win and the
/// durable record on disk matches that winner's exact sealed payload.
///
/// With `planted_shared_temp == true`, both peers omit the per-world nonce (`None`), reproducing
/// the pre-fix `.<leaf>.0.bstd-new` collision bug so the oracle must catch it deterministically.
fn c_probe_cross_world_sim_program(io: &Io, planted_shared_temp: bool) -> Result<(), FilesError> {
    let dir = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, RecordPlant::None);
    let alloc = blaze_own::testing::heap().alloc();

    let mut buf_a = [0u8; LONGEST];
    let mut buf_b = [0u8; LONGEST];
    let check_oracle = |store: &DirStore| -> Result<(), FilesError> {
        let mut a = [0u8; LONGEST];
        let mut b = [0u8; LONGEST];
        for k in 0..8u32 {
            let want_a = payload(k, &mut a);
            let want_b = payload(k + 16, &mut b);
            if let Some(held) = store.get_c_probe(&key(k), alloc)? {
                assert!(
                    held.as_slice() == want_a || held.as_slice() == want_b,
                    "cross-world publish oracle: key {k} corrupted on disk (len {})",
                    held.len()
                );
            }
        }
        Ok(())
    };
    check_oracle(&store)?;

    let seal_payload = |body: &[u8]| -> blaze_own::AVec<'_, u8> {
        let seal = blaze_ids::ContentId::of(super::SEAL, body).0;
        let mut out = blaze_own::AVec::with_capacity_in(body.len() + seal.len(), alloc);
        out.extend_from_slice(body);
        out.extend_from_slice(&seal);
        out
    };

    for k in 0..8u32 {
        let want_a = payload(k, &mut buf_a);
        let want_b = payload(k + 16, &mut buf_b);
        let sealed_a = seal_payload(want_a);
        let sealed_b = seal_payload(want_b);
        let name = DirStore::named(b"c-probe/", &key(k));
        let rel = bstd::path::Path::new(name.as_str()).map_err(|_| FilesError::NoTree)?;

        // Two independent peer worlds sharing the simulated disk, BOTH starting `fresh() == 0`.
        let (nonce_a, nonce_b) = if planted_shared_temp {
            (None, None)
        } else {
            (Some(0xa11c_e000_0000_0000 | u64::from(k)), Some(0xb0b0_f000_0000_0000 | u64::from(k)))
        };
        let peer_a = io.peer_world_for_test(nonce_a).ok_or(FilesError::NoTree)?;
        let peer_b = io.peer_world_for_test(nonce_b).ok_or(FilesError::NoTree)?;
        let cx_a = peer_a
            .cx(core::time::Duration::from_secs(30), bstd::mem::TASK_FRAME)
            .map_err(|_| FilesError::Stopped { name: DirStore::named(b"c-probe/", &key(k)) })?;
        let cx_b = peer_b
            .cx(core::time::Duration::from_secs(30), bstd::mem::TASK_FRAME)
            .map_err(|_| FilesError::Stopped { name: DirStore::named(b"c-probe/", &key(k)) })?;
        let dir_a = peer_a.need_fs().map_err(|_| FilesError::NoTree)?.root();
        let dir_b = peer_b.need_fs().map_err(|_| FilesError::NoTree)?.root();

        // Both peers stage concurrently BEFORE either calls `Effect::Link`.
        let staged_a = dir_a
            .stage_publish_if_absent(&cx_a, &rel, bstd::fs::Content::bytes(&sealed_a), bstd::fs::Durable::Yes)
            .map_err(|_| FilesError::NoTree)?;
        let staged_b = dir_b
            .stage_publish_if_absent(&cx_b, &rel, bstd::fs::Content::bytes(&sealed_b), bstd::fs::Durable::Yes)
            .map_err(|_| FilesError::NoTree)?;

        // Exercise both finish interleavings across keys.
        let (won_a, won_b) = if k % 2 == 0 {
            let a = dir_a
                .finish_publish_if_absent(&cx_a, staged_a, bstd::fs::Durable::Yes)
                .map_err(|_| FilesError::NoTree)?;
            let b = dir_b
                .finish_publish_if_absent(&cx_b, staged_b, bstd::fs::Durable::Yes)
                .map_err(|_| FilesError::NoTree)?;
            (a, b)
        } else {
            let b = dir_b
                .finish_publish_if_absent(&cx_b, staged_b, bstd::fs::Durable::Yes)
                .map_err(|_| FilesError::NoTree)?;
            let a = dir_a
                .finish_publish_if_absent(&cx_a, staged_a, bstd::fs::Durable::Yes)
                .map_err(|_| FilesError::NoTree)?;
            (a, b)
        };

        assert!(!(won_a && won_b), "cross-world publish oracle: both peers won on key {k}");
        let held = store.get_c_probe(&key(k), alloc)?;
        if won_a {
            assert!(
                held.as_deref() == Some(want_a),
                "cross-world publish oracle: peer A won on key {k} but disk holds another payload"
            );
        }
        if won_b {
            assert!(
                held.as_deref() == Some(want_b),
                "cross-world publish oracle: peer B won on key {k} but disk holds another payload"
            );
        }
    }
    check_oracle(&store)
}

#[test]
fn c_probe_cross_world_dst_survives_crashes_and_isolates_peers() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_cross_world_publish",
        Seeds::DEFAULT.from(0, 300),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_cross_world_sim_program(&io, false),
    ) {
        Ok(passed) => {
            assert!(passed.counts.cuts > 100, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 100, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn c_probe_cross_world_dst_catches_planted_shared_temp_collision() {
    match seeded(
        "blaze-proc-macro",
        module_path!(),
        "c_probe_cross_world_planted_collision",
        Seeds::DEFAULT.from(0, 32),
        Given::NOTHING,
        |_: &Cx, io: Io| c_probe_cross_world_sim_program(&io, true),
    ) {
        Ok(passed) => panic!("planted shared-temp collision was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            assert!(
                said.contains("cross-world publish oracle"),
                "planted shared-temp collision failed otherwise: {said}"
            );
        }
    }
}

#[test]
fn c_probe_repair_preserves_both_op_and_unlock_errors() {
    let name = super::RecordName::from_ascii(b"c-probe/test");
    let lock_name = super::RecordName::from_ascii(b"c-probe/.repair.lock");
    let combined = super::combine_repair_and_unlock(
        Err(FilesError::OtherBytes { name }),
        Err(FilesError::Stopped { name: lock_name }),
    )
    .expect_err("combined repair + unlock error");
    assert!(matches!(combined, FilesError::RepairAndUnlock { .. }));
    let text = std::format!("{combined}");
    assert!(
        text.contains("holds other bytes") && text.contains("releasing repair lock also failed"),
        "combined repair/unlock error must report both causes: {text}"
    );
    assert!(super::combine_repair_and_unlock(Ok(()), Ok(())).is_ok());
    assert!(matches!(
        super::combine_repair_and_unlock(Ok(()), Err(FilesError::Stopped { name: lock_name })),
        Err(FilesError::Stopped { .. })
    ));
}
