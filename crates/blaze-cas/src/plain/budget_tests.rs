//! Byte budget, soft/hard watermark hysteresis, and root-level LRU eviction proven on the
//! deterministic simulation (rules 43 and 64):
//!
//! 1. Under the high watermark (85% of budget), nothing is evicted early, and after eviction
//!    down toward the low watermark (75%), growth between 75% and 85% triggers no eviction
//!    (hysteresis).
//! 2. Over budget, oldest-touched roots are evicted first (using their existing expiry and
//!    generation timestamps, zero per-read writes), while roots touched by the current
//!    command, permanent (`NEVER`) roots, and roots under active leases are never evicted, and
//!    the active (newest) toolchain root (`tool.*`) is preserved last (after answers, programs,
//!    and older toolchains).
//! 3. Concurrent lock-free readers keep reading their mapped generation cleanly throughout
//!    root-LRU eviction and sweeping; concurrent writers that reuse records from an evicted
//!    root carry them forward with zero holed roots; power cuts at every completion step of an
//!    evicting wave preserve every surviving root byte-for-byte.
//! 4. Controls plant one bug each (`LruEvictsNewest`, `LruEvictsTouched`,
//!    `LruEvictsToolchainFirst`, `CarrySkipped`) and prove the oracle catches every one.

use core::time::Duration;

use blaze_core::{
    Driver, FileName,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_data::Cursor;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::Frame;

use super::{
    core::{
        DEFAULT_MAX_STORE_BYTES, GRACE_NS, Liveness, Plain, PlainConfig, Plant, ROOT_LIFE_NS, TAGS, WATERMARK_HIGH_PCT, WATERMARK_LOW_PCT, watermark_bytes,
    },
    settings::Settings,
    test_root,
};
use crate::Key;

const TAG: Tag = Tag(1);
const HOUR: Duration = Duration::from_secs(60 * 60);
const MINUTE: Duration = Duration::from_secs(60);

fn dir() -> FileName {
    FileName::new("plain").expect("a name")
}

fn cfg(n: u64, boot: u64, cap_bytes: u64, durable: bool, root: &Frame) -> PlainConfig<&Frame> {
    let settings = Settings::default();
    PlainConfig {
        dir: dir(),
        tags: (n + 1) * TAGS,
        cap_bytes,
        boot,
        nonce: 7000 + n,
        durable,
        machine: PlainConfig::empty_machine(root),
        writes: settings.writes(root),
        reads: settings.reads(root),
        liveness: Liveness::Reached,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain budget test key", &n.to_le_bytes()))
}

/// Deterministic payload of `size` bytes uniquely keyed by `n`.
fn value(n: u32, size: usize, root: &Frame) -> blaze_own::Vec<u8, &Frame> {
    let mut out = blaze_own::Vec::with_capacity_in(size, root);
    for i in 0..size {
        out.push(u8::try_from((usize::try_from(n).unwrap_or(0).wrapping_mul(31).wrapping_add(i)) & 0xff).unwrap_or(0));
    }
    out
}

/// Drives one store to `ready()` on `sim`.
macro_rules! settle {
    ($sim:expr, $store:expr, $effects:expr) => {{
        let sim: &mut Sim = $sim;
        let store: &mut Plain<&Frame> = $store;
        let effects = $effects;
        let mut done = core::default::Default::default();
        let mut settled = false;
        for _ in 0..100_000u32 {
            if !effects.is_empty() {
                sim.submit(effects);
            }
            if store.ready() && effects.is_empty() && sim.in_flight() == 0 {
                settled = true;
                break;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(3600)));
            let wall = sim.wall_ns();
            for c in done.drain(..) {
                if store.owns(c.tag) {
                    store.complete(c, wall, effects);
                }
            }
            if store.broken().is_some() {
                settled = true;
                break;
            }
        }
        assert!(settled, "store did not settle (phase: {})", store.what());
    }};
}

fn open_whole<'f>(sim: &mut Sim, n: u64, boot: u64, cap_bytes: u64, durable: bool, plant: Plant, root: &'f Frame) -> Plain<&'f Frame> {
    let mut store = Plain::planted(cfg(n, boot, cap_bytes, durable, root), root, plant);
    let mut effects = core::default::Default::default();
    store.open_whole(&mut effects);
    settle!(sim, &mut store, &mut effects);
    assert!(store.broken().is_none(), "open failed: {:?}", store.broken());
    store.handed();
    store
}

fn commit_ok(sim: &mut Sim, store: &mut Plain<&Frame>) {
    let mut effects = core::default::Default::default();
    let wall = sim.wall_ns();
    store.commit(&[], wall, &mut effects).expect("commit starts");
    settle!(sim, store, &mut effects);
    assert!(store.broken().is_none(), "commit broke: {:?}", store.broken());
    match store.take_committed() {
        Some(Ok(_)) => {}
        other => panic!("commit failed: {other:?}"),
    }
}

fn compact_until_clean(sim: &mut Sim, store: &mut Plain<&Frame>, max_slices: u32) -> (u32, u32) {
    let mut effects = core::default::Default::default();
    let mut slices = 0u32;
    let mut evicted = 0u32;
    for _ in 0..max_slices {
        assert!(store.compact(&mut effects), "compact starts");
        settle!(sim, store, &mut effects);
        assert!(store.broken().is_none(), "compact broke: {:?}", store.broken());
        let sliced = store.take_sliced().expect("sliced outcome");
        slices += 1;
        evicted += sliced.evicted_roots;
        if !sliced.more || sliced.aborted || (sliced.segments == 0 && sliced.evicted_roots == 0 && sliced.removed == 0) {
            break;
        }
    }
    (slices, evicted)
}

/// Verifies that every surviving unexpired root in `store` reads back every key it names
/// with byte-for-byte equality against `model`.
fn verify_surviving_roots(store: &Plain<&Frame>, now_ns: u64, model: &Map<[u8; 32], (u32, usize), BuildKeyHasher, &Frame>, root: &Frame) -> u32 {
    let mut checked_records = 0u32;
    for (name, root_key, exp) in store.roots_expiring() {
        if exp <= now_ns {
            continue;
        }
        let got_root = store.get(&root_key).unwrap_or_else(|| panic!("surviving root '{name}' record missing"));
        for chunk in got_root.bytes.chunks_exact(32) {
            let raw: [u8; 32] = chunk.try_into().expect("32 bytes");
            let rec_key = Key(ContentId(raw));
            let got = store.get(&rec_key).unwrap_or_else(|| panic!("surviving root '{name}' has hole at {rec_key:?}"));
            if let Some(&(n, size)) = model.get(&raw) {
                let want = value(n, size, root);
                assert_eq!(got.bytes, want.as_slice(), "corrupt bytes for surviving root '{name}' record {n}");
                checked_records += 1;
            }
        }
    }
    checked_records
}

#[test]
fn settings_budget_defaults_and_watermarks() {
    let root = test_root();
    assert_eq!(Settings::default().cap_bytes(), DEFAULT_MAX_STORE_BYTES);
    assert_eq!(Settings { max_store_bytes: Some(0), ..Settings::default() }.cap_bytes(), u64::MAX);
    assert_eq!(Settings { cap_mib: Some(0), ..Settings::default() }.cap_bytes(), u64::MAX);
    assert_eq!(
        Settings { cap_mib: Some(10), max_store_bytes: Some(4096), ..Settings::default() }.cap_bytes(),
        4096,
        "max_store_bytes takes precedence over cap_mib"
    );
    assert_eq!(Settings { cap_mib: Some(16), ..Settings::default() }.cap_bytes(), 16 << 20);

    // JSON round-trip with max_store_bytes.
    let json = br#"{"max_store_bytes": 8589934592, "durable": false}"#;
    let parsed = blaze_json::read_in(json, blaze_json::Limits::DEFAULT, &root).expect("parse json");
    let settings = Cursor::root(&parsed, &root).get::<Settings>().expect("decode Settings");
    assert_eq!(settings.max_store_bytes, Some(8 << 30));
    assert_eq!(settings.cap_bytes(), 8 << 30);

    // Watermarks: exact 85% / 75%, unlimited stays u64::MAX.
    assert_eq!(watermark_bytes(u64::MAX, WATERMARK_HIGH_PCT), u64::MAX);
    assert_eq!(watermark_bytes(u64::MAX, WATERMARK_LOW_PCT), u64::MAX);
    assert_eq!(watermark_bytes(100_000, WATERMARK_HIGH_PCT), 85_000);
    assert_eq!(watermark_bytes(100_000, WATERMARK_LOW_PCT), 75_000);
    assert_eq!(watermark_bytes(32 << 30, WATERMARK_LOW_PCT), 24 << 30);
}

/// (a) Under budget nothing is evicted early; over 85% high watermark oldest roots are evicted
/// down to <= 75% low watermark; subsequent growth between 75% and 85% triggers NO eviction
/// (hysteresis).
#[test]
fn under_budget_evicts_nothing_and_hysteresis_holds() {
    let root = test_root();
    let mut sim = Sim::new(101, Faults::default());
    // Budget = 20 KiB -> high watermark = 17,408 B (85%), low watermark = 15,360 B (75%).
    const CAP: u64 = 20 << 10;
    let high = watermark_bytes(CAP, WATERMARK_HIGH_PCT);
    let low = watermark_bytes(CAP, WATERMARK_LOW_PCT);
    let mut model: Map<[u8; 32], (u32, usize), BuildKeyHasher, &Frame> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &root, capacity: None });

    // Write 4 roots of ~3 KiB each (~13 KiB total < 17,408 B high watermark).
    for i in 0..4u32 {
        sim.advance(HOUR);
        let mut s = open_whole(&mut sim, u64::from(i) + 1, 1, CAP, false, Plant::None, &root);
        let k = key(i);
        let v = value(i, 3000, &root);
        model.insert(k.0.0, (i, 3000));
        s.put(k, Namespace::LOCAL, TAG, &v).expect("put");
        let name = if i == 0 { "r0" } else if i == 1 { "r1" } else if i == 2 { "r2" } else { "r3" };
        s.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    // Under budget: compact evicts NOTHING and all 4 roots survive.
    let mut gc = open_whole(&mut sim, 10, 1, CAP, false, Plant::None, &root);
    assert!(gc.bytes() <= high, "expected under high watermark: {} vs {high}", gc.bytes());
    assert!(!gc.over_budget(), "under budget must not report over_budget");
    let (_, evicted) = compact_until_clean(&mut sim, &mut gc, 8);
    assert_eq!(evicted, 0, "under budget must evict 0 roots");
    assert_eq!(gc.roots().len(), 4, "all 4 roots survive under budget");

    // Push over high watermark: add 3 more roots of 3 KiB each (~22 KiB > 17,408 B).
    for i in 4..7u32 {
        sim.advance(HOUR);
        let mut s = open_whole(&mut sim, u64::from(i) + 20, 1, CAP, false, Plant::None, &root);
        let k = key(i);
        let v = value(i, 3000, &root);
        model.insert(k.0.0, (i, 3000));
        s.put(k, Namespace::LOCAL, TAG, &v).expect("put");
        let name = if i == 4 { "r4" } else if i == 5 { "r5" } else { "r6" };
        s.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    let mut gc = open_whole(&mut sim, 30, 1, CAP, false, Plant::None, &root);
    assert!(gc.over_budget(), "should be over high watermark: {} vs {high}", gc.bytes());
    let plan = gc.plan_gc(sim.wall_ns()).expect("plan_gc");
    assert!(plan.lru_evict_roots >= 2, "dry-run plan should report LRU evictions: {plan:?}");
    let (_, evicted) = compact_until_clean(&mut sim, &mut gc, 16);
    assert!(evicted >= 2, "expected oldest roots evicted, got {evicted}");
    assert!(gc.bytes() <= low + 1024, "after eviction + sweep bytes ({}) should be near/below low watermark ({low})", gc.bytes());
    assert!(gc.root_named("r0").is_none(), "oldest root r0 must be evicted");
    assert!(gc.root_named("r1").is_none(), "second oldest root r1 must be evicted");
    assert!(gc.root_named("r6").is_some(), "newest root r6 must survive");
    assert!(gc.root_named("r5").is_some(), "recent root r5 must survive");
    verify_surviving_roots(&gc, sim.wall_ns(), &model, &root);

    // Hysteresis: add 1 small 1 KiB root staying between 75% and 85% -> 0 evictions!
    sim.advance(HOUR);
    let mut s = open_whole(&mut sim, 40, 1, CAP, false, Plant::None, &root);
    let k7 = key(7);
    let v7 = value(7, 1024, &root);
    model.insert(k7.0.0, (7, 1024));
    s.put(k7, Namespace::LOCAL, TAG, &v7).expect("put");
    s.root_for("r7", &[k7], ROOT_LIFE_NS).expect("root");
    s.release_lease();
    commit_ok(&mut sim, &mut s);

    let mut gc2 = open_whole(&mut sim, 41, 1, CAP, false, Plant::None, &root);
    if gc2.bytes() <= high {
        let (_, evicted2) = compact_until_clean(&mut sim, &mut gc2, 8);
        assert_eq!(evicted2, 0, "hysteresis band (between 75% and 85%) must not evict");
    }
    verify_surviving_roots(&gc2, sim.wall_ns(), &model, &root);
}

/// Runs the priority-order scenario under `plant` and returns which roots survived:
/// - `perm`: permanent NEVER root (must ALWAYS survive)
/// - `tool.old`: older toolchain root (evicted AFTER regular answers, BEFORE active toolchain)
/// - `tool.active`: active (newest) toolchain root (preserved LAST)
/// - `ans.0`: oldest regular root, RENEWED/TOUCHED by the compacting process (must ALWAYS survive)
/// - `ans.1`, `ans.2`: older regular roots (must be evicted FIRST)
/// - `ans.3`: newest regular root (must survive ahead of `ans.1`/`ans.2`)
/// - `leased`: root created while another process holds an active lease (must ALWAYS survive)
fn run_priority_scenario(cap_bytes: u64, plant: Plant, root: &Frame) -> (bool, bool, bool, bool, bool, bool, bool, bool) {
    let mut sim = Sim::new(202, Faults::default());
    let mut n_proc = 1u64;

    let mut add = |sim: &mut Sim, name: &str, rec_id: u32, size: usize, life_ns: u64, hold_lease: bool| {
        sim.advance(HOUR);
        let mut s = open_whole(sim, n_proc, 1, cap_bytes, false, Plant::None, root);
        n_proc += 1;
        let k = key(rec_id);
        let v = value(rec_id, size, root);
        s.put(k, Namespace::LOCAL, TAG, &v).expect("put");
        s.root_for(name, &[k], life_ns).expect("root");
        if !hold_lease {
            s.release_lease();
        }
        commit_ok(sim, &mut s);
    };

    // 1. Permanent marker root (NEVER).
    add(&mut sim, "perm", 10, 512, u64::MAX, false);
    // 2. Old toolchain root (oldest timestamp, same life as active toolchain so active is strictly newer).
    add(&mut sim, "tool.old", 11, 3200, ROOT_LIFE_NS, false);
    // 3. Active (newer) toolchain root (same ROOT_LIFE_NS as answers, earlier timestamp than ans.*,
    //    so its preservation comes specifically from the ActiveToolchain tier!).
    add(&mut sim, "tool.active", 12, 3200, ROOT_LIFE_NS, false);
    // 4. Regular answers from oldest to newest.
    add(&mut sim, "ans.0", 13, 3200, ROOT_LIFE_NS, false);
    add(&mut sim, "ans.1", 14, 3200, ROOT_LIFE_NS, false);
    add(&mut sim, "ans.2", 15, 3200, ROOT_LIFE_NS, false);
    add(&mut sim, "ans.3", 16, 3200, ROOT_LIFE_NS, false);
    // 5. A process opens, writes a record & root while keeping its lease active!
    add(&mut sim, "leased", 17, 3200, ROOT_LIFE_NS, true);

    // Compacting process touches (renews) `ans.0` (which is Fresh — no flip needed!),
    // then compacts under `plant`.
    let mut gc = open_whole(&mut sim, 99, 1, cap_bytes, false, plant, root);
    let renewed = gc.renew_root("ans.0", ROOT_LIFE_NS, sim.wall_ns());
    assert!(renewed.is_live(), "ans.0 must be live when touched");
    let (_, evicted) = compact_until_clean(&mut sim, &mut gc, 16);
    assert!(evicted > 0, "expected evictions over budget under {plant:?}");

    (
        gc.root_named("perm").is_some(),
        gc.root_named("tool.active").is_some(),
        gc.root_named("tool.old").is_some(),
        gc.root_named("ans.0").is_some(),
        gc.root_named("ans.1").is_some(),
        gc.root_named("ans.2").is_some(),
        gc.root_named("ans.3").is_some(),
        gc.root_named("leased").is_some(),
    )
}

/// (b) Over budget: oldest regular roots evicted first; active/touched/permanent/leased roots
/// survive; older toolchain evicted before active toolchain; active toolchain preserved last.
/// Planted-bug controls prove every single rule!
#[test]
fn over_budget_evicts_oldest_preserves_touched_leased_and_active_toolchain_last() {
    let root = test_root();
    // Budget 24 KiB (high ~20.9 KiB, low ~18.4 KiB): total ~23.7 KiB -> evicts 2 oldest regular roots (ans.1, ans.2), keeping ans.3 + toolchains!
    const CAP_KEEP_NEWEST: u64 = 24 << 10;
    let (perm, tool_active, tool_old, ans0, ans1, ans2, ans3, leased) = run_priority_scenario(CAP_KEEP_NEWEST, Plant::None, &root);
    assert!(perm, "permanent NEVER root must survive");
    assert!(leased, "root under active lease must survive");
    assert!(ans0, "root touched by current command (even Fresh no-op renewal) must survive");
    assert!(tool_active, "active toolchain root must be preserved last");
    assert!(tool_old, "old toolchain survives when evicting 2 regular roots suffices");
    assert!(!ans1, "oldest untouched regular root ans.1 must be evicted first");
    assert!(!ans2, "second oldest untouched regular root ans.2 must be evicted");
    assert!(ans3, "newest regular root ans.3 must survive ahead of older ans.1/ans.2");

    // Tighter budget 16 KiB (low ~12.3 KiB): must evict ALL untouched regular roots AND tool.old,
    // while tool.active (and perm, ans.0, leased) STILL survive!
    const CAP_EVICT_OLD_TOOL: u64 = 16 << 10;
    let (perm2, tool_active2, tool_old2, ans0_2, ans1_2, ans2_2, ans3_2, leased2) = run_priority_scenario(CAP_EVICT_OLD_TOOL, Plant::None, &root);
    assert!(perm2 && leased2 && ans0_2, "protected roots always survive");
    assert!(!ans1_2 && !ans2_2 && !ans3_2, "all untouched regular roots evicted before toolchains");
    assert!(!tool_old2, "old toolchain evicted before active toolchain");
    assert!(tool_active2, "active toolchain root preserved last");

    // Control 1: Plant::LruEvictsNewest evicts ans.3 instead of ans.1 -> caught!
    let (_, _, _, _, p_ans1, _, p_ans3, _) = run_priority_scenario(CAP_KEEP_NEWEST, Plant::LruEvictsNewest, &root);
    assert!(!p_ans3 && p_ans1, "control LruEvictsNewest must evict newest ans.3 and keep oldest ans.1");

    // Control 2: Plant::LruEvictsTouched evicts ans.0 (touched by current command) -> caught!
    let (_, _, _, p_ans0, _, _, _, _) = run_priority_scenario(CAP_KEEP_NEWEST, Plant::LruEvictsTouched, &root);
    assert!(!p_ans0, "control LruEvictsTouched must evict touched root ans.0");

    // Control 3: Plant::LruEvictsToolchainFirst evicts tool.active early -> caught!
    let (_, p_tool_active, _, _, _, _, _, _) = run_priority_scenario(CAP_KEEP_NEWEST, Plant::LruEvictsToolchainFirst, &root);
    assert!(!p_tool_active, "control LruEvictsToolchainFirst must evict active toolchain early");
}

/// Indexed roots sharing common records charge shared bytes to the newest surviving root:
/// evicting the older indexed root drops its exclusive records while keeping every shared
/// record intact for the newer root.
#[test]
fn indexed_roots_charge_shared_records_to_newest_root() {
    let root = test_root();
    let mut sim = Sim::new(303, Faults::default());
    // Budget = 12 KiB -> high watermark ~10.4 KiB, low watermark ~9.2 KiB.
    const CAP: u64 = 12 << 10;
    let mut model: Map<[u8; 32], (u32, usize), BuildKeyHasher, &Frame> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &root, capacity: None });

    // Shared records: 2 x 2500 B = 5000 B.
    // Old-only records: 2 x 2500 B = 5000 B.
    // New-only records: 1 x 2500 B = 2500 B.
    // Total live = ~12.5 KiB (> high watermark 10.4 KiB).
    // Evicting old_prog frees 5000 B -> surviving new_prog (shared + new_only) = ~7.5 KiB (<= 9.2 KiB)!
    {
        let mut s = open_whole(&mut sim, 1, 1, CAP, false, Plant::None, &root);
        let mut shared = blaze_own::Vec::new_in(&root);
        for id in 0..2u32 {
            let k = key(id);
            model.insert(k.0.0, (id, 2500));
            s.put(k, Namespace::LOCAL, TAG, &value(id, 2500, &root)).expect("put shared");
            shared.push(k);
        }
        let mut old_only = blaze_own::Vec::new_in(&root);
        for id in 10..12u32 {
            let k = key(id);
            model.insert(k.0.0, (id, 2500));
            s.put(k, Namespace::LOCAL, TAG, &value(id, 2500, &root)).expect("put old");
            old_only.push(k);
        }
        let shared_idx = s.index(&shared).expect("shared index");
        let old_idx = s.index(&old_only).expect("old index");
        s.root_through("code.old", &[shared_idx, old_idx], ROOT_LIFE_NS).expect("old root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    sim.advance(HOUR);
    {
        let mut s = open_whole(&mut sim, 2, 1, CAP, false, Plant::None, &root);
        let shared = [key(0), key(1)];
        let k_new = key(20);
        model.insert(k_new.0.0, (20, 2500));
        s.put(k_new, Namespace::LOCAL, TAG, &value(20, 2500, &root)).expect("put new");
        let shared_idx = s.index(&shared).expect("shared index");
        let new_idx = s.index(&[k_new]).expect("new index");
        s.root_through("code.new", &[shared_idx, new_idx], ROOT_LIFE_NS).expect("new root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    let mut gc = open_whole(&mut sim, 3, 1, CAP, false, Plant::None, &root);
    let (_, evicted) = compact_until_clean(&mut sim, &mut gc, 16);
    assert_eq!(evicted, 1, "only code.old should be evicted");
    assert!(gc.root_named("code.old").is_none(), "code.old evicted");
    assert!(gc.root_named("code.new").is_some(), "code.new survived");
    // Shared records and new-only record must all be readable; old-only records must be swept.
    for id in [0u32, 1, 20] {
        let got = gc.get(&key(id)).unwrap_or_else(|| panic!("record {id} must survive"));
        assert_eq!(got.bytes, value(id, 2500, &root).as_slice());
    }
    for id in [10u32, 11] {
        assert!(gc.get(&key(id)).is_none(), "old-only record {id} must be reclaimed");
    }
}

/// (c) Crash-safety, lock-free concurrent readers, and concurrent writers reusing records from
/// an evicted root:
/// - A lock-free reader opened before eviction keeps reading every record of its generation
///   with zero corruption while GC evicts roots, sweeps segments, and advances past GRACE_NS.
/// - A concurrent writer opened before eviction that sets a new root reusing a record from the
///   evicted root automatically carries that record forward (zero holed roots).
/// - Power cuts (`sim.crash()` + next boot) at every completion step of an evicting slice
///   recover with every surviving root intact byte-for-byte.
#[test]
fn concurrent_readers_writers_and_power_cuts_under_budget_lru() {
    let root = test_root();
    let mut sim = Sim::new(404, Faults::default());
    const CAP: u64 = 16 << 10;
    let mut model: Map<[u8; 32], (u32, usize), BuildKeyHasher, &Frame> = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &root, capacity: None });

    // Seed 5 roots (4 KiB each = ~20 KiB > 16 KiB cap) durably.
    let names = ["c0", "c1", "c2", "c3", "c4"];
    for (i, name) in names.iter().enumerate() {
        sim.advance(HOUR);
        let id = u32::try_from(i).unwrap_or(0);
        let mut s = open_whole(&mut sim, u64::from(id) + 1, 1, CAP, true, Plant::None, &root);
        let k = key(id);
        model.insert(k.0.0, (id, 4000));
        s.put(k, Namespace::LOCAL, TAG, &value(id, 4000, &root)).expect("put");
        s.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    // 1. Open a lock-free concurrent reader AND a concurrent writer on the pre-eviction generation!
    let reader = open_whole(&mut sim, 20, 1, CAP, true, Plant::None, &root);
    let mut writer = open_whole(&mut sim, 21, 1, CAP, true, Plant::None, &root);
    // Writer reuses record 0 (currently held ONLY by the oldest root `c0`!) in a brand-new root `c_reuser`.
    assert!(writer.get(&key(0)).is_some(), "writer sees record 0");
    writer.root_for("c_reuser", &[key(0)], ROOT_LIFE_NS).expect("writer sets c_reuser");

    // 2. Meanwhile, GC runs on another process, evicts `c0` (and `c1`), and sweeps record 0 out!
    let mut gc = open_whole(&mut sim, 22, 1, CAP, true, Plant::None, &root);
    let (_, evicted) = compact_until_clean(&mut sim, &mut gc, 16);
    assert!(evicted >= 2, "GC evicted oldest roots: {evicted}");
    assert!(gc.root_named("c0").is_none(), "c0 evicted by GC");
    assert!(gc.get(&key(0)).is_none(), "record 0 swept by GC");

    // Lock-free reader STILL reads every single pre-eviction record intact with zero locks!
    for id in 0..5u32 {
        let got = reader.get(&key(id)).unwrap_or_else(|| panic!("lock-free reader lost record {id}"));
        assert_eq!(got.bytes, value(id, 4000, &root).as_slice(), "lock-free reader got corrupt bytes for {id}");
    }

    // 3. Now the concurrent writer commits `c_reuser` naming record 0 (which GC already swept!).
    //    Our concurrent-eviction carry guard MUST carry record 0 forward!
    writer.release_lease();
    commit_ok(&mut sim, &mut writer);
    assert!(writer.carried() >= 1, "concurrent writer must carry record 0 forward after LRU eviction + sweep");

    // Fresh reader verifies `c_reuser` and every surviving root are 100% intact with zero holes!
    let check = open_whole(&mut sim, 23, 1, CAP, true, Plant::None, &root);
    assert!(check.root_named("c_reuser").is_some(), "c_reuser committed");
    assert_eq!(check.get(&key(0)).expect("carried record 0 readable").bytes, value(0, 4000, &root).as_slice());
    verify_surviving_roots(&check, sim.wall_ns(), &model, &root);

    // 4. Advance past GRACE_NS so retired segments unlink, and verify lock-free reader STILL reads
    //    its already-mapped generation cleanly, while power cuts at EVERY completion step of an
    //    eviction slice recover cleanly!
    sim.advance(Duration::from_nanos(GRACE_NS) + MINUTE);
    for id in 0..5u32 {
        assert_eq!(reader.get(&key(id)).expect("mapped bytes outlive unlink").bytes, value(id, 4000, &root).as_slice());
    }

    // Power cuts (`sim.crash()` + next boot) at EVERY completion step of a durable eviction slice!
    let mut total_cuts = 0u32;
    for cut_at in 0..80u32 {
        let mut branch = Sim::new(4040, Faults::default());
        for (i, name) in names.iter().enumerate() {
            branch.advance(HOUR);
            let id = u32::try_from(i).unwrap_or(0);
            let mut s = open_whole(&mut branch, u64::from(id) + 1, 1, CAP, true, Plant::None, &root);
            let k = key(id);
            s.put(k, Namespace::LOCAL, TAG, &value(id, 4000, &root)).expect("put");
            s.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
            s.release_lease();
            commit_ok(&mut branch, &mut s);
        }
        let mut gc_cut = open_whole(&mut branch, 200 + u64::from(cut_at), 1, CAP, true, Plant::None, &root);
        let mut effects = core::default::Default::default();
        assert!(gc_cut.compact(&mut effects));
        let mut done = core::default::Default::default();
        let mut seen = 0u32;
        let mut hit = false;
        while !gc_cut.ready() && gc_cut.broken().is_none() {
            if !effects.is_empty() {
                branch.submit(&mut effects);
            }
            if branch.in_flight() == 0 {
                break;
            }
            let now = branch.now();
            branch.poll(&mut done, Deadline::after(now, Duration::from_secs(3600)));
            let wall = branch.wall_ns();
            for c in done.drain(..) {
                if seen == cut_at {
                    hit = true;
                    break;
                }
                seen += 1;
                if gc_cut.owns(c.tag) {
                    gc_cut.complete(c, wall, &mut effects);
                }
            }
            if hit {
                break;
            }
        }
        if !hit {
            break;
        }
        total_cuts += 1;
        drop(gc_cut);
        branch.crash();
        // Next boot opens and checks every surviving root byte-for-byte!
        let recovered = open_whole(&mut branch, 500 + u64::from(cut_at), 2, CAP, true, Plant::None, &root);
        verify_surviving_roots(&recovered, branch.wall_ns(), &model, &root);
    }
    assert!(total_cuts >= 10, "expected power cuts across eviction slice steps, ran {total_cuts}");
}

/// Control for concurrent-eviction carry: planting `CarrySkipped` while a concurrent writer
/// reuses a record from an LRU-evicted root produces a holed root that our oracle catches.
#[test]
fn carry_skipped_control_catches_concurrent_eviction_hole() {
    let root = test_root();
    let mut sim = Sim::new(505, Faults::default());
    const CAP: u64 = 16 << 10;

    for (i, name) in ["d0", "d1", "d2", "d3", "d4"].iter().enumerate() {
        sim.advance(HOUR);
        let id = u32::try_from(i).unwrap_or(0);
        let mut s = open_whole(&mut sim, u64::from(id) + 1, 1, CAP, false, Plant::None, &root);
        let k = key(id);
        s.put(k, Namespace::LOCAL, TAG, &value(id, 4000, &root)).expect("put");
        s.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    let mut writer = open_whole(&mut sim, 30, 1, CAP, false, Plant::CarrySkipped, &root);
    writer.root_for("d_reuser", &[key(0)], ROOT_LIFE_NS).expect("writer sets d_reuser");

    let mut gc = open_whole(&mut sim, 31, 1, CAP, false, Plant::None, &root);
    compact_until_clean(&mut sim, &mut gc, 16);
    assert!(gc.get(&key(0)).is_none(), "record 0 swept");

    writer.release_lease();
    commit_ok(&mut sim, &mut writer);
    let check = open_whole(&mut sim, 32, 1, CAP, false, Plant::None, &root);
    assert!(check.root_named("d_reuser").is_some(), "d_reuser committed without carry");
    assert!(check.get(&key(0)).is_none(), "CarrySkipped control must leave a hole at record 0");
}

/// Regression test for `Driven::run` stall clock:
/// 1. Slow CPU work inside `Plain::complete(Done::Listed)` (exceeding `STALL = 120s`) must NOT
///    cause the next newly submitted I/O's `poll()` to see an already-expired deadline and return
///    a false `StoreError::Stalled { waited_for: "compacting" }`.
/// 2. A genuine I/O stall (no completions arriving for > `STALL`) still returns
///    `StoreError::Stalled { waited_for: "compacting" }`.
#[test]
fn cpu_work_inside_on_done_does_not_trip_false_stall_while_real_io_stall_does() {
    use core::cell::Cell;
    use blaze_core::{Boot, Completion, Done, Effect, NoBoot};
    use blaze_keyed::store::StoreError;
    use super::driven::Driven;

    /// Wraps `Sim` and simulates >120s of CPU work inside `Plain::complete` after `Done::Listed`
    /// is delivered, while rejecting any `poll()` whose deadline is already expired (just like a
    /// real OS poller).
    struct SlowCpuDriver {
        inner: Sim,
        extra_clock: Cell<Duration>,
        arm_cpu_stall: Cell<bool>,
        triggered: Cell<u32>,
    }

    impl Driver for SlowCpuDriver {
        fn submit(&mut self, effects: &mut Vec<Effect>) {
            self.inner.submit(effects);
        }

        fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
            if until.passed(self.now()) {
                return 0;
            }
            let inner_now = self.inner.now();
            let inner_until = Deadline::after(inner_now, Duration::from_secs(3600));
            let n = self.inner.poll(out, inner_until);
            if out.iter().any(|c| matches!(c.done, Done::Listed(_))) {
                self.arm_cpu_stall.set(true);
            }
            n
        }

        fn in_flight(&self) -> usize {
            self.inner.in_flight()
        }

        fn now(&self) -> Duration {
            self.inner.now() + self.extra_clock.get()
        }

        fn wall_ns(&self) -> u64 {
            // Called right before `for completion in self.done.drain(..) { complete(); }`:
            // advancing the clock by 180s (> STALL = 120s) models slow CPU work inside
            // `listed_compact`. With the old bug (`since` refreshed BEFORE `complete`), the
            // next `poll()` would receive an already-expired deadline and falsely stall.
            if self.arm_cpu_stall.replace(false) {
                self.extra_clock.set(self.extra_clock.get() + Duration::from_secs(180));
                self.triggered.set(self.triggered.get() + 1);
            }
            self.inner.wall_ns()
        }

        fn boot(&self) -> Result<Boot, NoBoot> {
            self.inner.boot()
        }
    }

    /// Driver whose I/O genuinely hangs once `hang` is armed, advancing the clock past `STALL`
    /// while returning 0 completions.
    struct HungDriver {
        inner: Sim,
        extra_clock: Cell<Duration>,
        hang: bool,
    }

    impl Driver for HungDriver {
        fn submit(&mut self, effects: &mut Vec<Effect>) {
            self.inner.submit(effects);
        }

        fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
            if self.hang {
                // Genuine I/O hang: clock advances past deadline with 0 completions.
                self.extra_clock.set(self.extra_clock.get() + Duration::from_secs(130));
                return 0;
            }
            let _ = until;
            let now = self.inner.now();
            self.inner.poll(out, Deadline::after(now, Duration::from_secs(3600)))
        }

        fn in_flight(&self) -> usize {
            self.inner.in_flight()
        }

        fn now(&self) -> Duration {
            self.inner.now() + self.extra_clock.get()
        }

        fn wall_ns(&self) -> u64 {
            self.inner.wall_ns()
        }

        fn boot(&self) -> Result<Boot, NoBoot> {
            self.inner.boot()
        }
    }

    let root = test_root();
    const CAP: u64 = 16 << 10;

    let seed_sim = || {
        let mut sim = Sim::new(606, Faults::default());
        // Seed several segments with dead/evictable data so compaction does real copy + write I/O.
        for i in 0..8u32 {
            sim.advance(HOUR);
            let mut s = open_whole(&mut sim, u64::from(i) + 1, 1, CAP, false, Plant::None, &root);
            let k = key(i);
            s.put(k, Namespace::LOCAL, TAG, &value(i, 3000, &root)).expect("put");
            if i >= 4 {
                let name = format!("live-{i}");
                s.root_for(&name, &[k], ROOT_LIFE_NS).expect("root");
            }
            s.release_lease();
            commit_ok(&mut sim, &mut s);
        }
        sim
    };

    // Part 1: >120s CPU work inside `complete(Done::Listed)` must NOT trip false Stalled.
    let slow = SlowCpuDriver {
        inner: seed_sim(),
        extra_clock: Cell::new(Duration::ZERO),
        arm_cpu_stall: Cell::new(false),
        triggered: Cell::new(0),
    };
    let mut driven = Driven::with(slow, "/sim".as_ref(), cfg(50, 1, CAP, false, &root), &root).expect("driven opens");
    let slices = driven.compact_foreground(16).expect("CPU work in on_done must not trip false Stalled");
    assert!(driven.driver().triggered.get() >= 1, "SlowCpuDriver must have injected >120s CPU delay after Done::Listed");
    assert!(!slices.is_empty(), "compaction ran at least one slice");
    // Verify surviving live roots are still readable.
    for i in 4..8u32 {
        let got = driven.get(&key(i)).expect("get").expect("live key present");
        assert_eq!(driven.got(got).bytes, value(i, 3000, &root).as_slice());
    }

    // Part 2 (control): a genuine I/O hang still trips `StoreError::Stalled { waited_for: "compacting" }`.
    let hung = HungDriver { inner: seed_sim(), extra_clock: Cell::new(Duration::ZERO), hang: false };
    let mut driven_hung = Driven::with(hung, "/sim".as_ref(), cfg(51, 1, CAP, false, &root), &root).expect("driven opens");
    driven_hung.map_all().expect("map_all before hang");
    driven_hung.driver().hang = true;
    let err = driven_hung.compact_foreground(4).expect_err("genuine I/O hang must fail with Stalled");
    assert!(
        matches!(err, StoreError::Stalled { waited_for: "compacting" }),
        "expected Stalled {{ waited_for: \"compacting\" }}, got {err:?}"
    );
}

/// Multi-slice foreground compaction on a store with many segments, direct roots, indexed roots,
/// expired roots, a permanent root, an active toolchain root, and dead bytes reclaims dead bytes
/// quickly and soundly, and every surviving root remains readable through both `Driven` and
/// `Snapshot` (exercising lazy Bloom CRC verification and `map_all`).
#[test]
fn multi_slice_compact_foreground_reclaims_many_segments_and_roots_soundly() {
    use super::{driven::Driven, format::NEVER, lent::Lent};

    let root = test_root();
    let mut sim = Sim::new(707, Faults::default());
    // Finite cap so both root-LRU eviction, sweeping, and tiered merges all run across slices.
    const CAP: u64 = 128 << 10;
    let mut model: Map<[u8; 32], (u32, usize), BuildKeyHasher, &Frame> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: &root, capacity: Some(512) });
    let mut next_id = 1u32;

    // 1. 10 soon-to-expire roots + dead unrooted records (20 segments).
    for r in 0..10u32 {
        sim.advance(MINUTE);
        let mut s = open_whole(&mut sim, u64::from(r) + 1, 1, u64::MAX, false, Plant::None, &root);
        let mut rkeys = blaze_own::Vec::new_in(&root);
        for _ in 0..4u32 {
            let id = next_id;
            next_id += 1;
            let k = key(id);
            let sz = 1200 + usize::try_from((id * 37) % 600).unwrap_or(0);
            s.put(k, Namespace::LOCAL, TAG, &value(id, sz, &root)).expect("put");
            model.insert(k.0.0, (id, sz));
            rkeys.push(k);
        }
        // Unrooted dead record in the same segment.
        let dead = key(next_id);
        next_id += 1;
        s.put(dead, Namespace::LOCAL, TAG, &value(99_000 + r, 2500, &root)).expect("put dead");
        let name = format!("expiring-{r}");
        s.root_for(&name, &rkeys, 5 * MINUTE.as_nanos() as u64).expect("expiring root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    // Advance past the 10 expiring roots' lifetime.
    sim.advance(HOUR);

    // 2. 12 indexed roots (`index` + `root_through`), some sharing keys, plus 20 direct roots,
    //    plus 1 permanent (`NEVER`) root and 1 active `tool.active` root (34 more segments).
    let mut shared_keys = blaze_own::Vec::new_in(&root);
    {
        let mut s = open_whole(&mut sim, 40, 1, u64::MAX, false, Plant::None, &root);
        for _ in 0..8u32 {
            let id = next_id;
            next_id += 1;
            let k = key(id);
            let sz = 1500;
            s.put(k, Namespace::LOCAL, TAG, &value(id, sz, &root)).expect("put shared");
            model.insert(k.0.0, (id, sz));
            shared_keys.push(k);
        }
        s.root_for("perm.keep", &shared_keys, NEVER).expect("permanent root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    for r in 0..12u32 {
        sim.advance(MINUTE);
        let mut s = open_whole(&mut sim, 50 + u64::from(r), 1, u64::MAX, false, Plant::None, &root);
        let mut idx_keys = blaze_own::Vec::new_in(&root);
        idx_keys.extend_from_slice(&shared_keys[..2]);
        for _ in 0..5u32 {
            let id = next_id;
            next_id += 1;
            let k = key(id);
            let sz = 1100 + usize::try_from((id * 53) % 500).unwrap_or(0);
            s.put(k, Namespace::LOCAL, TAG, &value(id, sz, &root)).expect("put");
            model.insert(k.0.0, (id, sz));
            idx_keys.push(k);
        }
        let idx = s.index(&idx_keys).expect("index");
        let name = format!("indexed-{r}");
        s.root_through(&name, &[idx], ROOT_LIFE_NS).expect("root_through");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    for r in 0..20u32 {
        sim.advance(MINUTE);
        let mut s = open_whole(&mut sim, 80 + u64::from(r), 1, u64::MAX, false, Plant::None, &root);
        let mut rkeys = blaze_own::Vec::new_in(&root);
        for _ in 0..3u32 {
            let id = next_id;
            next_id += 1;
            let k = key(id);
            let sz = 1400;
            s.put(k, Namespace::LOCAL, TAG, &value(id, sz, &root)).expect("put");
            model.insert(k.0.0, (id, sz));
            rkeys.push(k);
        }
        let name = format!("direct-{r}");
        s.root_for(&name, &rkeys, ROOT_LIFE_NS).expect("direct root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    // Active toolchain root (newest `tool.*` must survive LRU eviction).
    sim.advance(MINUTE);
    {
        let mut s = open_whole(&mut sim, 120, 1, u64::MAX, false, Plant::None, &root);
        let id = next_id;
        let k = key(id);
        s.put(k, Namespace::LOCAL, TAG, &value(id, 4096, &root)).expect("put tool");
        model.insert(k.0.0, (id, 4096));
        s.root_for("tool.active", &[k], ROOT_LIFE_NS).expect("tool root");
        s.release_lease();
        commit_ok(&mut sim, &mut s);
    }

    // Run multi-slice `compact_foreground` under the finite cap.
    let mut gc = Driven::with(sim, "/sim".as_ref(), cfg(200, 1, CAP, false, &root), &root).expect("gc opens");
    let bytes_before = gc.total_bytes();
    let segs_before = gc.core().segments().len();
    assert!(segs_before >= 40, "expected many segments before GC, got {segs_before}");
    assert!(bytes_before > CAP, "expected store over cap before GC ({bytes_before} <= {CAP})");

    let ran = gc.compact_foreground(256).expect("multi-slice compact_foreground succeeds");
    assert!(!ran.is_empty(), "expected at least one compaction slice");
    let total_evicted: u32 = ran.iter().map(|s| s.evicted_roots).sum();
    assert!(total_evicted > 0, "expected over-budget LRU root eviction");

    let bytes_after = gc.total_bytes();
    let segs_after = gc.core().segments().len();
    assert!(bytes_after < bytes_before, "GC must reclaim dead bytes ({bytes_before} -> {bytes_after})");
    assert!(bytes_after <= CAP, "GC must bring store under cap ({bytes_after} <= {CAP})");
    assert!(segs_after < segs_before, "GC must reduce segment count ({segs_before} -> {segs_after})");

    // Permanent root and active toolchain root must survive.
    assert!(gc.core().root_named("perm.keep").is_some(), "permanent root must survive");
    assert!(gc.core().root_named("tool.active").is_some(), "active toolchain root must survive");

    // Advance past GRACE_NS and run one more foreground pass to unlink retired segments.
    gc.driver().advance(Duration::from_nanos(GRACE_NS + 1_000_000));
    let unlink_ran = gc.compact_foreground(16).expect("post-grace unlink compact succeeds");
    let unlinked: u32 = unlink_ran.iter().map(|s| s.removed).sum();
    assert!(unlinked > 0, "post-grace pass must unlink retired segment files");

    gc.seal_foreground().expect("seal_foreground succeeds");
    let sim = gc.into_driver();

    // Fresh reader opens lazily, verifies every surviving root via both `Plain` and `Snapshot`
    // (exercising `map_all`, `Snapshot::of`, and lazy atomic Bloom CRC verification).
    let lent = Lent::new(
        Driven::with(sim, "/sim".as_ref(), cfg(201, 1, CAP, false, &root), &root).expect("reader opens"),
        &root,
    );
    let snap = lent.snapshot_in(&root).expect("snapshot succeeds");
    let now_ns = lent.wall_ns();
    let mut checked = 0u32;
    for (name, root_key, exp) in lent.roots_expiring() {
        if exp <= now_ns {
            continue;
        }
        let root_got = lent.get(&root_key).expect("lent get root").unwrap_or_else(|| panic!("surviving root '{name}' missing in Lent"));
        let root_snap = snap.get(&root_key).expect("snap get root").unwrap_or_else(|| panic!("surviving root '{name}' missing in Snapshot"));
        assert_eq!(root_got.bytes, root_snap, "root '{name}' bytes differ between Lent and Snapshot");
        let check_leaf = |raw: [u8; 32]| {
            let k = Key(ContentId(raw));
            let Some(&(id, sz)) = model.get(&raw) else {
                return;
            };
            let want = value(id, sz, &root);
            let g1 = lent.get(&k).expect("lent get").unwrap_or_else(|| panic!("key {id} of '{name}' missing in Lent"));
            let g2 = snap.get(&k).expect("snap get").unwrap_or_else(|| panic!("key {id} of '{name}' missing in Snapshot"));
            assert_eq!(g1.bytes, want.as_slice(), "Lent mismatch for key {id}");
            assert_eq!(g2, want.as_slice(), "Snapshot mismatch for key {id}");
        };
        for chunk in root_snap.chunks_exact(32) {
            let raw: [u8; 32] = chunk.try_into().expect("32 bytes");
            if model.contains_key(&raw) {
                check_leaf(raw);
                checked += 1;
            } else {
                // Indexed root: follow one level to its index record.
                let idx_bytes = snap.get(&Key(ContentId(raw))).expect("snap get index").expect("index record present");
                for key_chunk in idx_bytes.chunks_exact(32) {
                    let key_raw: [u8; 32] = key_chunk.try_into().expect("32 bytes");
                    check_leaf(key_raw);
                    checked += 1;
                }
            }
        }
    }
    assert!(checked >= 10, "expected >= 10 live records verified across surviving roots, got {checked}");
}
