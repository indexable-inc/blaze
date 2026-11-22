//! Tests for the greenfield `bstd` upgrades:
//! - Lock-free trace fast-path (`Limits::QUIET` & full-trace `TraceGate`)
//! - `Cx::scratch` RAII temporary frame rewind
//! - `Bytes::with_capacity_in`, `capacity`, `as_mut_slice`
//! - Zero-alloc `Pending<'buf, C>` (`File::{read_into, write_from}`, `Conn::send_vectored`)
//! - `Resolver::{lookup, lookup_into}` via `Effect::Resolve` and `SimHost::names`
//! - Cancellation-aware `Lock::lock(cx)`, `read_lock(cx)`, `write_lock(cx)`
//! - Linear group-commit file transactions (`Dir::txn`)
//! - Cost-weighted LPT (`par::for_each_weighted`) and guided dynamic (`par::for_each_dynamic`)

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU32, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use super::{path, root, world};
use crate::{
    Plant, Stopped,
    fs::{Content, FsKind, Unsynced},
    io::{Io, Limits, SimWorld},
    mem::{Bytes, TASK_FRAME},
    net::{Addr, HostPort, NetKind},
    par::{self, ParError},
    sync::{Lock, RwLock, read_lock, write_lock},
};

#[test]
fn quiet_limits_and_full_trace_gate_count_drops_without_losing_accuracy() {
    // 1. Limits::QUIET records nothing and reports 0 dropped / 0 records.
    let quiet = Io::simulated(SimWorld::new(1, Limits::QUIET));
    let qcx = root(&quiet);
    for _ in 0..32 {
        let s = qcx.span("hot");
        s.event("tick");
    }
    quiet.trace(|t| {
        assert_eq!(t.records().len(), 0);
        assert_eq!(t.dropped(), 0);
    });

    // 2. Small `trace_most` fills up, closes the lock-free `TraceGate`, and folds exact drops.
    let small = Io::simulated(SimWorld::new(2, Limits { trace_most: 2, ..Limits::USUAL }));
    let scx = root(&small);
    // 1 root span ("main") + 10 events = 11 total records: 2 kept, 9 dropped via TraceGate.
    for _ in 0..10 {
        scx.event("ping");
    }
    small.trace(|t| {
        assert_eq!(t.records().len(), 2);
        assert_eq!(t.dropped(), 9);
    });
    // A second trace read sees the folded total without double-counting.
    small.trace(|t| {
        assert_eq!(t.records().len(), 2);
        assert_eq!(t.dropped(), 9);
    });
}

#[test]
fn cx_scratch_allocates_in_local_frame_and_inherits_cancellation() {
    let io = world(3, Plant::None);
    let cx = root(&io);
    let sum = cx
        .scratch(64 * 1024, |scx| {
            let mut buf = Bytes::with_capacity_in(scx.frame(), "test.scratch", 128).expect("capacity fits");
            assert!(buf.capacity() >= 128);
            buf.try_extend(b"hello world").expect("fits in scratch");
            buf.as_mut_slice()[0] = b'H';
            assert_eq!(buf.as_slice(), b"Hello world");
            buf.clear();
            assert_eq!(buf.len(), 0);
            assert!(buf.capacity() >= 128);
            42usize
        })
        .expect("scratch succeeds");
    assert_eq!(sum, 42);

    // Scratch inside a cancelled scope returns `Stopped::Cancelled` without running `f`.
    let mut ran = false;
    let outcome = cx.scope_with(|s| {
        s.cancel();
        let t = s.may_overlap("cancelled-scratch", TASK_FRAME, |tcx| {
            let res = tcx.scratch(16 * 1024, |_| {
                ran = true;
            });
            assert!(matches!(res, Err(Stopped::Cancelled { .. })), "expected Cancelled, got {res:?}");
            Ok::<_, std::convert::Infallible>(())
        });
        let _ = t.join(s.cx());
    });
    assert!(outcome.is_ok());
    assert!(!ran, "scratch closure must not run when cx is already cancelled");
}

#[test]
fn resolver_uses_effect_resolve_and_simhost_override_and_lookup_into() {
    let io = world(4, Plant::None);
    let cx = root(&io);
    let net = io.need_net().expect("sim has net");
    let resolver = net.resolver();

    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    let ip_override = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9));

    // Register multiple addresses on the simulated driver (`Effect::Resolve`).
    io.with_sim(|sim| {
        sim.name("multi.example", &[ip1, ip2]);
    });

    let hp = HostPort::new("multi.example", 443).expect("valid host:port");
    let mut out = Vec::new_in(cx.frame());
    let n = resolver.lookup_into(&cx, &hp, &mut out).expect("resolves via Effect::Resolve");
    assert_eq!(n, 2);
    assert_eq!(out.as_slice(), &[SocketAddr::new(ip1, 443), SocketAddr::new(ip2, 443)]);
    assert_eq!(resolver.lookup(&cx, &hp).expect("first addr"), SocketAddr::new(ip1, 443));

    // Unknown name returns NoSuchName.
    let missing = HostPort::new("missing.example", 80).expect("valid host:port");
    let err = resolver.lookup(&cx, &missing).expect_err("unknown host");
    assert_eq!(err.kind, NetKind::NoSuchName);

    // SimHost::names overrides Effect::Resolve when configured.
    io.script(|host| {
        host.names.push(("override.example".to_owned(), ip_override));
    });
    let ov = HostPort::new("override.example", 8080).expect("valid host:port");
    assert_eq!(resolver.lookup(&cx, &ov).expect("simhost override"), SocketAddr::new(ip_override, 8080));
}

#[test]
fn conn_send_vectored_and_zero_alloc_pending_round_trip() {
    let io = world(5, Plant::None);
    let cx = root(&io);
    let net = io.need_net().expect("sim has net");
    let addr = Addr::Tcp(HostPort::new("127.0.0.1", 9091).expect("valid host:port"));

    let mut listener = match net.listen(&cx, &addr) {
        Ok(l) => l,
        Err(e) => panic!("listen: {e}"),
    };
    let client = match net.connect(&cx, &addr) {
        Ok(c) => c,
        Err(e) => panic!("connect: {e}"),
    };
    let server = match listener.accept(&cx) {
        Ok(conn) => conn,
        Err(e) => panic!("accept: {e}"),
    };

    // Send multiple slices via `send_vectored` (skips empty slices cleanly).
    let sent = client.send_vectored(&cx, &[b"blaze-", b"", b"vectored-", b"io"]);
    let mut recv = [0u8; 32];
    let pending = server.read_into(&mut recv);
    let n = pending.wait(&cx);

    let _ = client.finish(&cx);
    let _ = server.finish(&cx);
    let _ = listener.close(&cx);

    sent.expect("send_vectored succeeds");
    let n = n.expect("read_into succeeds");
    assert_eq!(&recv[..n], b"blaze-vectored-io");
}

#[test]
fn rwlock_respects_deadlines_and_locks_wake_promptly_on_cancellation() {
    let io = world(6, Plant::None);
    let cx = root(&io);

    // 1. `read_lock` and `write_lock` respect finite deadlines.
    let rw = RwLock::new("test.rw", 10u32);
    {
        let _w = write_lock(&rw, &cx).expect("uncontended write lock");
        let brief = cx.until(io.need_clock().expect("clock").after(Duration::from_millis(5)));
        assert!(matches!(read_lock(&rw, &brief), Err(Stopped::Deadline { .. })));
        assert!(matches!(write_lock(&rw, &brief), Err(Stopped::Deadline { .. })));
    }
    {
        let _r = read_lock(&rw, &cx).expect("uncontended read lock");
        let _r2 = read_lock(&rw, &cx).expect("concurrent readers allowed");
        let brief = cx.until(io.need_clock().expect("clock").after(Duration::from_millis(5)));
        assert!(matches!(write_lock(&rw, &brief), Err(Stopped::Deadline { .. })));
    }

    // 2. Mid-wait cancellation on a held `Lock` and `RwLock` wakes promptly on a real worker
    // pool instead of waiting for the 30s deadline.
    let dir = std::env::temp_dir().join(format!("bstd-upgrades-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let native = crate::test::native(&dir).expect("native io");

    let lock = Arc::new(Lock::new("test.cancel.lock", 0u32));
    let rw_arc = Arc::new(RwLock::new("test.cancel.rw", 0u32));
    let _held_lock = lock.try_lock().expect("hold lock");
    let _held_rw = rw_arc.try_write().expect("hold rw");

    let entered = Arc::new(AtomicBool::new(false));
    let lock_res = Arc::new(Lock::new("test.result", None::<Stopped>));
    let rw_res = Arc::new(Lock::new("test.rw.result", None::<Stopped>));

    let elapsed = native.with_test_workers(NonZeroU32::new(3).expect("3 workers"), |ncx| {
        let scope = ncx.scope();
        let l1 = Arc::clone(&lock);
        let r1 = Arc::clone(&lock_res);
        let e1 = Arc::clone(&entered);
        let t_lock = scope.may_overlap("wait-lock", TASK_FRAME, move |tcx| {
            e1.store(true, Ordering::Release);
            let res = l1.lock(tcx).err();
            if let Some(mut g) = r1.try_lock() {
                *g = res;
            }
            Ok::<_, std::convert::Infallible>(())
        });

        let rw2 = Arc::clone(&rw_arc);
        let r2 = Arc::clone(&rw_res);
        let t_rw = scope.may_overlap("wait-rw", TASK_FRAME, move |tcx| {
            let res = read_lock(&rw2, tcx).err();
            if let Some(mut g) = r2.try_lock() {
                *g = res;
            }
            Ok::<_, std::convert::Infallible>(())
        });

        // Wait briefly so the worker tasks enter their lock waits, then cancel the scope.
        let deadline_spin = Instant::now() + Duration::from_millis(500);
        while !entered.load(Ordering::Acquire) && Instant::now() < deadline_spin {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(5));
        let cancel_at = Instant::now();
        scope.cancel();

        let _ = t_lock.join(ncx);
        let _ = t_rw.join(ncx);
        let _ = scope.join(ncx);
        cancel_at.elapsed()
    });

    drop(native);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(elapsed < Duration::from_secs(2), "cancellation took too long: {elapsed:?}");
    assert!(
        matches!(lock_res.try_lock().and_then(|g| g.clone()), Some(Stopped::Cancelled { .. })),
        "Lock::lock must wake with Stopped::Cancelled"
    );
    assert!(
        matches!(rw_res.try_lock().and_then(|g| g.clone()), Some(Stopped::Cancelled { .. })),
        "read_lock must wake with Stopped::Cancelled"
    );
}

#[test]
fn fs_txn_group_commits_with_one_dir_sync_and_survives_crash() {
    let io = world(7, Plant::None);
    let cx = root(&io);
    let dir = io.need_fs().expect("fs").open_dir(&cx, &path("data")).expect("dir");

    // Seed two existing files for rename + remove testing.
    dir.publish_synced(&cx, &path("old_name"), Content::bytes(b"renamed-body")).expect("seed rename");
    dir.publish_synced(&cx, &path("doomed"), Content::bytes(b"bye")).expect("seed remove");

    // Stage 5 publishes (with 1 duplicate target overwrite), 1 rename, 1 remove -> 1 dir sync.
    let mut txn = dir.txn();
    assert!(txn.is_empty());
    txn.publish(&cx, &path("f0"), Content::bytes(b"stale")).expect("stage f0 stale");
    txn.publish(&cx, &path("f0"), Content::bytes(b"v0")).expect("replace f0 staged");
    txn.publish(&cx, &path("f1"), Content::bytes(b"v1")).expect("stage f1");
    txn.publish(&cx, &path("f2"), Content::bytes(b"v2")).expect("stage f2");
    txn.publish(&cx, &path("f3"), Content::bytes(b"v3")).expect("stage f3");
    txn.publish(&cx, &path("f4"), Content::bytes(b"v4")).expect("stage f4");
    txn.rename_replacing(&path("old_name"), &path("new_name"));
    txn.remove(&path("doomed"));
    assert_eq!(txn.len(), 7);

    // Before commit, none of the targets are visible yet.
    let mut probe = cx.bytes();
    assert_eq!(dir.read_all(&cx, &path("f0"), &mut probe).expect_err("not visible before commit").kind, FsKind::NotFound);

    let committed = txn.commit_synced(&cx).expect("commit_synced");
    assert_eq!(committed.published, 5);
    assert_eq!(committed.renamed, 1);
    assert_eq!(committed.removed, 1);
    assert_eq!(committed.dirs_synced, 1);

    // Crash the simulated disk and verify every single change survived cleanly!
    assert!(io.crash());
    for (name, want) in [
        ("f0", &b"v0"[..]),
        ("f1", b"v1"),
        ("f2", b"v2"),
        ("f3", b"v3"),
        ("f4", b"v4"),
        ("new_name", b"renamed-body"),
    ] {
        let mut got = cx.bytes();
        dir.read_all(&cx, &path(name), &mut got).expect("survives crash");
        assert_eq!(got.as_slice(), want, "file {name} mismatch after crash");
    }
    let mut gone = cx.bytes();
    assert_eq!(dir.read_all(&cx, &path("doomed"), &mut gone).expect_err("removed survives crash").kind, FsKind::NotFound);

    // Verify `commit_barrier` syncs the directory once.
    let mut btxn = dir.txn();
    btxn.publish(&cx, &path("b0"), Content::bytes(b"barrier-0")).expect("stage b0");
    btxn.publish(&cx, &path("b1"), Content::bytes(b"barrier-1")).expect("stage b1");
    let bcom = btxn.commit_barrier(&cx).expect("commit_barrier");
    assert_eq!(bcom.published, 2);
    assert_eq!(bcom.dirs_synced, 1);

    // Verify `commit_unsynced` performs zero device waits.
    let waits_before = io.disk().expect("sim disk").device_waits;
    let mut utxn = dir.txn();
    utxn.publish(&cx, &path("u0"), Content::bytes(b"unsynced-0")).expect("stage u0");
    let ucom = utxn.commit_unsynced(&cx, Unsynced::No).expect("commit_unsynced");
    assert_eq!(ucom.published, 1);
    assert_eq!(ucom.dirs_synced, 0);
    assert_eq!(io.disk().expect("sim disk").device_waits, waits_before);

    // Verify `abort` leaves targets untouched.
    let mut atxn = dir.txn();
    atxn.publish(&cx, &path("never"), Content::bytes(b"aborted")).expect("stage aborted");
    atxn.abort(&cx).expect("abort");
    let mut check = cx.bytes();
    assert_eq!(dir.read_all(&cx, &path("never"), &mut check).expect_err("aborted target absent").kind, FsKind::NotFound);
}

#[test]
fn par_weighted_and_dynamic_visit_every_item_exactly_once() {
    let io = world(8, Plant::None);
    let cx = root(&io);
    let scope = cx.scope();

    // 1. `for_each_weighted` with skewed item weights visits every item exactly once.
    let weights: Vec<u64> = (0..64).map(|i| if i % 13 == 0 { 1000 } else { 10 }).collect();
    let mut visits = vec![0u32; weights.len()];
    let res = par::for_each_weighted(
        &cx,
        &scope,
        &mut visits[..],
        &weights,
        50,
        64 * 1024,
        |chunk: &mut [u32]| {
            for v in chunk {
                *v += 1;
            }
        },
    );
    assert!(res.is_ok(), "for_each_weighted failed: {res:?}");
    assert!(visits.iter().all(|&v| v == 1), "every weighted item must be visited once");

    // Ragged weights length is refused with `ParError::Ragged`.
    let mut short = vec![0u32; 4];
    let ragged = par::for_each_weighted(&cx, &scope, &mut short[..], &weights, 50, 64 * 1024, |_| {});
    assert!(matches!(ragged, Err(ParError::Ragged { first: 4, other: 64 })));

    // 2. `for_each_dynamic` visits every index exactly once on simulated pool.
    let items: Vec<u64> = (0..128).collect();
    let seen: Vec<AtomicUsize> = (0..items.len()).map(|_| AtomicUsize::new(0)).collect();
    let dyn_res = par::for_each_dynamic(
        &cx,
        &scope,
        &items,
        NonZeroUsize::new(3).expect("non-zero grain"),
        64 * 1024,
        |_wcx, start, slice| {
            for (off, &val) in slice.iter().enumerate() {
                assert_eq!(val as usize, start + off);
                seen[start + off].fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    let joined = scope.join(&cx);
    assert!(dyn_res.is_ok(), "for_each_dynamic failed: {dyn_res:?}");
    assert!(joined.is_ok());
    assert!(seen.iter().all(|c| c.load(Ordering::Relaxed) == 1), "every dynamic item visited once");

    // Also run `for_each_dynamic` on a real 4-worker thread pool!
    let dir = std::env::temp_dir().join(format!("bstd-upgrades-par-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let native = crate::test::native(&dir).expect("native io");
    let nseen: Vec<AtomicUsize> = (0..256).map(|_| AtomicUsize::new(0)).collect();
    let nitems: Vec<usize> = (0..256).collect();
    let (ndyn, njoined) = native.with_test_workers(NonZeroU32::new(4).expect("4 workers"), |ncx| {
        let nscope = ncx.scope();
        let ndyn = par::for_each_dynamic(
            ncx,
            &nscope,
            &nitems,
            NonZeroUsize::new(2).expect("non-zero"),
            64 * 1024,
            |_wcx, start, slice| {
                for (off, &val) in slice.iter().enumerate() {
                    assert_eq!(val, start + off);
                    nseen[start + off].fetch_add(1, Ordering::Relaxed);
                }
            },
        );
        let njoined = nscope.join(ncx);
        (ndyn, njoined)
    });
    drop(native);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(ndyn.is_ok(), "native for_each_dynamic failed: {ndyn:?}");
    assert!(njoined.is_ok());
    assert!(nseen.iter().all(|c| c.load(Ordering::Relaxed) == 1));
}
