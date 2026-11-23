//! Unit and deterministic simulation tests for `blaze gc`, budget environment variables,
//! non-blocking exclusive GC lock, dry-run mode, and old-format store cleanup.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{CACHEDIR_SIGNATURE, CACHEDIR_TAG, MIN_MAX_STORE_BYTES, ROOT_LIFE_NS, Settings};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::{
    AUX_EXPIRY_NS, AuxSweepPlant, DEFAULT_BUDGET, GcError, GcOpts, ORPHAN_GRACE_NS, PidProbe, PidState,
    VIEWER_DEAD_GRACE_NS, parse_args, parse_budget_values, parse_meta_pid, parse_pid_prefix, run_with,
    should_sweep_run, sweep_aux_with, try_acquire_gc_lock,
};
use crate::CliError;
use crate::store::plain_place::{Facts, STORE_DIR, STORE_ROOT};
use crate::store::Home;

const TEST_TAG: Tag = Tag(1);

fn key(n: u32) -> CasKey {
    CasKey(ContentId::of("blaze cli gc test key", &n.to_le_bytes()))
}

fn payload(n: u32, size: usize) -> blaze_own::Vec<u8, blaze_own::Alloc<'static>> {
    let mut out = blaze_own::Vec::with_capacity_in(size, blaze_own::testing::heap().alloc());
    for i in 0..size {
        out.push(u8::try_from((usize::try_from(n).unwrap_or(0).wrapping_mul(29).wrapping_add(i)) & 0xff).unwrap_or(0));
    }
    out
}

#[test]
fn budget_env_parsing_and_clamping() {
    assert_eq!(parse_budget_values(None, None).expect("unset"), None);
    assert_eq!(parse_budget_values(Some(""), Some("  ")).expect("blank"), None);
    assert_eq!(Settings::default().cap_bytes(), DEFAULT_BUDGET);

    // 0 means unlimited.
    assert_eq!(parse_budget_values(Some("0"), None).expect("zero bytes"), Some(0));
    assert_eq!(parse_budget_values(None, Some("0")).expect("zero gb"), Some(0));
    assert_eq!(Settings { max_store_bytes: Some(0), ..Settings::default() }.cap_bytes(), u64::MAX);

    // Non-zero values clamp to MIN_MAX_STORE_BYTES (1 GiB).
    assert_eq!(parse_budget_values(Some("4096"), None).expect("clamped"), Some(MIN_MAX_STORE_BYTES));
    assert_eq!(parse_budget_values(None, Some("1")).expect("1 gb"), Some(1 << 30));
    assert_eq!(parse_budget_values(None, Some("64")).expect("64 gb"), Some(64 << 30));

    // BLAZE_STORE_MAX_BYTES takes precedence over BLAZE_STORE_MAX_GB.
    assert_eq!(parse_budget_values(Some("2147483648"), Some("64")).expect("bytes wins"), Some(2 << 30));

    // Invalid values return typed errors.
    assert!(matches!(parse_budget_values(Some("not-a-num"), None), Err(GcError::InvalidBudget { .. })));
    assert!(matches!(parse_budget_values(None, Some("-5")), Err(GcError::InvalidBudget { .. })));
}

#[test]
fn gc_cli_arg_parsing() {
    assert_eq!(parse_args(core::iter::empty()).expect("empty"), GcOpts::default());
    assert_eq!(
        parse_args(["--max-gb", "16", "--dry-run", "--all-formats"].into_iter()).expect("flags"),
        GcOpts { max_store_bytes: Some(16 << 30), dry_run: true, all_formats: true, background: false }
    );
    assert_eq!(
        parse_args(["--max-gb=0", "--background"].into_iter()).expect("bg unlimited"),
        GcOpts { max_store_bytes: Some(0), dry_run: false, all_formats: false, background: true }
    );
    assert_eq!(
        parse_args(["--max-bytes=16384"].into_iter()).expect("exact bytes"),
        GcOpts { max_store_bytes: Some(16384), dry_run: false, all_formats: false, background: false }
    );
    assert!(matches!(parse_args(["--unknown"].into_iter()), Err(CliError::Usage(_))));
    assert!(matches!(parse_args(["--max-gb", "abc"].into_iter()), Err(CliError::Gc(GcError::InvalidBudget { .. }))));
}

#[test]
fn gc_dry_run_and_budget_eviction_reclaims_oldest_roots_and_old_formats() {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let fs = io.need_fs().expect("fs");
    let host = io.need_host().expect("host");
    let clock = io.need_clock().expect("clock");
    let mut rng = io.need_rng().expect("rng");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let heap = blaze_own::testing::heap();

    let home = Home::of(&cx, &fs, Some("/h")).expect("home");
    // Seed 5 roots (~4 KiB each = ~20 KiB total) in the active f5s3 store.
    let names = ["ans.0", "ans.1", "ans.2", "ans.3", "ans.4"];
    for (i, name) in names.iter().enumerate() {
        let id = u32::try_from(i).unwrap_or(0);
        let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 100 + u64::from(id) }, heap.alloc()).expect("open");
        let k = key(id);
        let bytes = payload(id, 4000);
        lent.put(&k, Namespace::LOCAL, TEST_TAG, &bytes).expect("put");
        lent.root_for(name, &[k], ROOT_LIFE_NS).expect("root");
        lent.release_lease();
        lent.commit().expect("commit");
    }

    // Seed CACHEDIR.TAG, an old-format folder `f5s2`, and legacy format-4 files in `v2/store/`.
    let tag_path = Path::new(STORE_ROOT).expect("root").join(&Path::new(CACHEDIR_TAG).expect("tag"));
    home.dir().publish(&cx, &tag_path, Content::bytes(CACHEDIR_SIGNATURE), Durable::No).expect("tag");
    let f5s2_head = Path::new("v2/store/f5s2/head").expect("f5s2 head");
    home.dir().publish(&cx, &f5s2_head, Content::bytes(b"old-format-head"), Durable::No).expect("f5s2");
    let legacy_seg = Path::new("v2/store/seg-0000000000000001").expect("legacy seg");
    home.dir().publish(&cx, &legacy_seg, Content::bytes(b"legacy-v4-seg"), Durable::No).expect("legacy");

    // 1. Dry-run: reports evictions and old formats, mutates NOTHING!
    let dry = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: Some(14 << 10), dry_run: true, all_formats: true, background: false },
        &out,
        &err,
        heap,
    )
    .expect("dry run");
    assert!(dry.evicted_roots >= 2, "dry-run must plan LRU evictions: {dry:?}");
    assert_eq!(dry.other_forms_removed, 2, "dry-run must report f5s2 + legacy format-4: {dry:?}");
    assert!(home.dir().stat(&cx, &f5s2_head).expect("stat").is_some(), "dry-run must not remove f5s2");
    assert!(home.dir().stat(&cx, &legacy_seg).expect("stat").is_some(), "dry-run must not remove legacy seg");
    {
        let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 200 }, heap.alloc()).expect("open");
        assert_eq!(lent.roots().len(), 5, "dry-run must not drop any roots");
    }

    // 2. Default run without `--all-formats`: recent old-format stores stay within grace period,
    //    while active store shrinks under budget via root-LRU eviction!
    let live_gc = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: Some(14 << 10), dry_run: false, all_formats: false, background: false },
        &out,
        &err,
        heap,
    )
    .expect("gc run");
    assert!(!live_gc.skipped_locked);
    assert!(live_gc.evicted_roots >= 2, "gc must evict oldest roots: {live_gc:?}");
    assert!(live_gc.bytes_after < live_gc.bytes_before, "gc must shrink store bytes: {live_gc:?}");
    assert_eq!(live_gc.other_forms_removed, 0, "recent other-format stores stay within 30-day grace");
    assert!(home.dir().stat(&cx, &f5s2_head).expect("stat").is_some(), "f5s2 kept within grace");

    // Verify newest roots survived intact and oldest roots were evicted.
    {
        let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 201 }, heap.alloc()).expect("open");
        assert!(lent.root_named("ans.0").is_none(), "oldest root ans.0 evicted");
        assert!(lent.root_named("ans.4").is_some(), "newest root ans.4 survived");
        let got = lent.get(&key(4)).expect("get").expect("newest record readable");
        assert_eq!(got.bytes, payload(4, 4000).as_slice());
    }

    // 3. Run with `--all-formats`: removes f5s2 + legacy format-4 files while keeping active f5s3 + CACHEDIR.TAG!
    let all_gc = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: Some(14 << 10), dry_run: false, all_formats: true, background: false },
        &out,
        &err,
        heap,
    )
    .expect("gc --all-formats");
    assert_eq!(all_gc.other_forms_removed, 2, "all-formats removes both f5s2 and legacy format-4");
    assert!(home.dir().stat(&cx, &f5s2_head).expect("stat").is_none(), "f5s2 removed");
    assert!(home.dir().stat(&cx, &legacy_seg).expect("stat").is_none(), "legacy format-4 removed");
    assert!(home.dir().stat(&cx, &tag_path).expect("stat").is_some(), "CACHEDIR.TAG preserved");
    assert!(home.dir().stat(&cx, &Path::new(STORE_DIR).expect("store dir")).expect("stat").is_some(), "active f5s3 preserved");
}

#[test]
fn gc_lock_prevents_concurrent_background_gc() {
    let io = Io::simulated(SimWorld::new(72, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let fs = io.need_fs().expect("fs");
    let host = io.need_host().expect("host");
    let clock = io.need_clock().expect("clock");
    let mut rng = io.need_rng().expect("rng");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let heap = blaze_own::testing::heap();

    let home = Home::of(&cx, &fs, Some("/h")).expect("home");
    // Hold the exclusive GC lock as if another background GC is already running.
    let held = try_acquire_gc_lock(&cx, home.dir()).expect("acquire").expect("lock free");

    // Second background GC sees the lock contended and exits cleanly without blocking!
    let second = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: None, dry_run: false, all_formats: false, background: true },
        &out,
        &err,
        heap,
    )
    .expect("contended bg gc exits cleanly");
    assert!(second.skipped_locked, "concurrent background gc must skip when gc.lock is held");

    // Once released, the next GC acquires the lock and runs normally.
    drop(held);
    let after = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: None, dry_run: false, all_formats: false, background: true },
        &out,
        &err,
        heap,
    )
    .expect("after unlock gc runs");
    assert!(!after.skipped_locked, "gc must acquire lock once previous holder exits");
}

#[cfg(target_os = "macos")]
#[test]
fn background_gc_preserves_background_qos_and_restores_caller_class() {
    use blaze_core::threads::qos::{self, Qos};

    let (before_class, before_rel) = qos::this_thread().expect("read qos");
    let _restore = qos::Lowered::at_most(Qos::UserInteractive).ok();
    qos::set_this_thread(Qos::UserInitiated, 0).expect("set caller class");
    let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let fs = io.need_fs().expect("fs");
    let host = io.need_host().expect("host");
    let clock = io.need_clock().expect("clock");
    let mut rng = io.need_rng().expect("rng");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let heap = blaze_own::testing::heap();

    let summary = run_with(
        &cx,
        &fs,
        &host,
        &clock,
        &mut rng,
        Some("/h"),
        GcOpts { max_store_bytes: None, dry_run: false, all_formats: false, background: true },
        &out,
        &err,
        heap,
    )
    .expect("bg gc");
    let restored = qos::this_thread().expect("qos after").0;
    let _ = qos::set_this_thread(before_class.settable(), before_rel);
    assert!(!summary.skipped_locked);
    assert_eq!(restored, Qos::UserInitiated, "caller thread QoS must be restored after background gc");
}

struct FixedPids<'a>(&'a [u32]);

impl PidProbe for FixedPids<'_> {
    fn probe(&self, pid: u32) -> PidState {
        if self.0.contains(&pid) { PidState::Alive } else { PidState::Dead }
    }
}

#[test]
fn gc_sweeps_dead_runs_keeps_live_and_viewer_protected_with_planted_control() {
    use bstd::fs::{Content, Durable};

    assert_eq!(parse_meta_pid(b"pid=4242\npackage=foo\n"), Some(4242));
    assert_eq!(parse_meta_pid(b"pid=0\n"), None);
    assert_eq!(parse_pid_prefix("999-123456789"), Some(999));
    assert_eq!(parse_pid_prefix("notapid-1"), None);

    let io = Io::simulated(SimWorld::new(74, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let fs = io.need_fs().expect("fs");
    let clock = io.need_clock().expect("clock");
    let home = Home::of(&cx, &fs, Some("/h")).expect("home");
    let now_ns = clock.wall().as_ns();

    let put = |rel: &str, bytes: &[u8]| {
        home.dir().publish(&cx, &Path::new(rel).expect("path"), Content::bytes(bytes), Durable::No).expect("publish");
    };
    put("v2/runs/live-1111.telem", &[1u8; 64]);
    put("v2/runs/live-1111.meta", b"pid=1001\npackage=live\n");
    put("v2/runs/dead-2222.telem", &[2u8; 128]);
    put("v2/runs/dead-2222.meta", b"pid=2002\npackage=dead\n");
    put("v2/runs/orphan-3333.telem", &[3u8; 32]);
    put("v2/test/tmp/1001-100/0/a.txt", b"live");
    put("v2/test/tmp/2002-100/0/b.txt", b"dead-test");
    put("v2/run/records/oldrecord", &[9u8; 40]);

    let probe = FixedPids(&[1001]);

    // 1. Dry-run reports dead run + dead test tmp without deleting anything.
    let dry = sweep_aux_with(&cx, home.dir(), now_ns, &probe, true, AuxSweepPlant::None);
    assert_eq!(dry.files, 3, "dead .telem + dead .meta + dead test tmp file");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/dead-2222.telem").expect("p")).expect("s").is_some());

    // 2. Real sweep removes dead run + dead test tmp, keeps live run, live test tmp, fresh orphan, fresh record.
    let gone = sweep_aux_with(&cx, home.dir(), now_ns, &probe, false, AuxSweepPlant::None);
    assert_eq!(gone.files, 3);
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/live-1111.telem").expect("p")).expect("s").is_some(), "live telem kept");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/live-1111.meta").expect("p")).expect("s").is_some(), "live meta kept");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/dead-2222.telem").expect("p")).expect("s").is_none(), "dead telem swept");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/dead-2222.meta").expect("p")).expect("s").is_none(), "dead meta swept");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/orphan-3333.telem").expect("p")).expect("s").is_some(), "fresh orphan kept within grace");
    assert!(home.dir().stat(&cx, &Path::new("v2/test/tmp/1001-100/0/a.txt").expect("p")).expect("s").is_some(), "live test tmp kept");
    assert!(home.dir().stat(&cx, &Path::new("v2/test/tmp/2002-100").expect("p")).expect("s").is_none(), "dead test tmp dir swept");
    assert!(home.dir().stat(&cx, &Path::new("v2/run/records/oldrecord").expect("p")).expect("s").is_some(), "fresh record kept");

    // 3. Past orphan + expiry grace, orphan .telem and expired record are swept while live PID stays.
    let later = now_ns.saturating_add(AUX_EXPIRY_NS).saturating_add(ORPHAN_GRACE_NS);
    let gone_later = sweep_aux_with(&cx, home.dir(), later, &probe, false, AuxSweepPlant::None);
    assert_eq!(gone_later.files, 2, "expired orphan .telem + expired run record");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/orphan-3333.telem").expect("p")).expect("s").is_none());
    assert!(home.dir().stat(&cx, &Path::new("v2/run/records/oldrecord").expect("p")).expect("s").is_none());
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/live-1111.telem").expect("p")).expect("s").is_some(), "live telem still kept");

    // 4. Viewer-active grace check (pure decision): dead PID kept within VIEWER_DEAD_GRACE_NS when viewer holds lock, swept after.
    assert!(!should_sweep_run(Some(PidState::Dead), true, now_ns, now_ns + VIEWER_DEAD_GRACE_NS - 1, AuxSweepPlant::None));
    assert!(should_sweep_run(Some(PidState::Dead), true, now_ns, now_ns + VIEWER_DEAD_GRACE_NS, AuxSweepPlant::None));

    // 5. Planted bug control (rule 43): ignoring PidState::Alive deletes the live run.
    let planted = sweep_aux_with(&cx, home.dir(), later, &probe, false, AuxSweepPlant::IgnorePidAlive);
    assert!(planted.files >= 2, "planted bug must delete live run to prove oracle");
    assert!(home.dir().stat(&cx, &Path::new("v2/runs/live-1111.telem").expect("p")).expect("s").is_none());
}
