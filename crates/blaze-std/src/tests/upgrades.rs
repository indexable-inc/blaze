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

#[test]
fn pending_select2_wait_both_and_try_wait_multiplex_linear_ops() {
    use crate::pending::{Either, select2, wait_both};

    let io = world(9, Plant::None);
    let cx = root(&io);
    let dir = io.need_fs().expect("fs").open_dir(&cx, &path("data")).expect("dir");
    dir.publish_synced(&cx, &path("a"), Content::bytes(b"alpha-data")).expect("seed a");
    dir.publish_synced(&cx, &path("b"), Content::bytes(b"beta-data!")).expect("seed b");

    let ro = dir.read_only();
    let fa = ro.open(&cx, &path("a")).expect("open a");
    let fb = ro.open(&cx, &path("b")).expect("open b");

    // 1. `select2` completes whichever finishes first and hands back the other linear `Pending`.
    let mut buf_a = [0u8; 16];
    let mut buf_b = [0u8; 16];
    let pa = fa.read_into(&mut buf_a, 0);
    let pb = fb.read_into(&mut buf_b, 0);
    let (na, nb) = match select2(&cx, pa, pb) {
        Ok(Either::First((ra, pending_b))) => (ra.expect("read a"), pending_b.wait(&cx).expect("read b")),
        Ok(Either::Second((pending_a, rb))) => (pending_a.wait(&cx).expect("read a"), rb.expect("read b")),
        Err((stopped, pending_a, pending_b)) => {
            let _ = pending_a.wait(&cx);
            let _ = pending_b.wait(&cx);
            panic!("select2 stopped unexpectedly: {stopped:?}");
        }
    };
    assert_eq!(&buf_a[..na], b"alpha-data");
    assert_eq!(&buf_b[..nb], b"beta-data!");

    // 2. `wait_both` drives two concurrent reads to completion without spawning a task.
    let mut buf_a2 = [0u8; 16];
    let mut buf_b2 = [0u8; 16];
    let (ra2, rb2) = wait_both(&cx, fa.read_into(&mut buf_a2, 0), fb.read_into(&mut buf_b2, 0));
    assert_eq!(&buf_a2[..ra2.expect("ra2")], b"alpha-data");
    assert_eq!(&buf_b2[..rb2.expect("rb2")], b"beta-data!");

    // 3. `try_wait` returns `Err(self)` when not yet polled, then completes cleanly on `wait`.
    let mut buf_c = [0u8; 16];
    let pc = fa.read_into(&mut buf_c, 0);
    let nc = match pc.try_wait() {
        Ok(res) => res.expect("ready immediately"),
        Err(still_pending) => still_pending.wait(&cx).expect("wait after try_wait"),
    };
    assert_eq!(&buf_c[..nc], b"alpha-data");
}

#[test]
fn net_connect_with_and_open_native_quiet_limits() {
    use crate::net::{Ask, Linger, Tcp, TcpOptions};

    let io = world(10, Plant::None);
    let cx = root(&io);
    let net = io.need_net().expect("net");
    let addr = Addr::Tcp(HostPort::new("127.0.0.1", 9092).expect("valid host:port"));
    let opts = Tcp::new(TcpOptions { linger: Ask::WhereAvailable(Linger::Reset), ..TcpOptions::DEFAULT });

    let mut listener = match net.listen_with(&cx, &addr, opts.clone()) {
        Ok(l) => l,
        Err(e) => panic!("listen_with: {e}"),
    };
    let client = match net.connect_with(&cx, &addr, opts) {
        Ok(c) => c,
        Err(e) => panic!("connect_with: {e}"),
    };
    let server = match listener.accept(&cx) {
        Ok(s) => s,
        Err(e) => panic!("accept: {e}"),
    };
    let _ = client.finish(&cx);
    let _ = server.finish(&cx);
    let _ = listener.close(&cx);

    // `open_native` uses `Limits::QUIET` by default and `Limits::USUAL` when `BLAZE_TRACE=1`.
    let dir = std::env::temp_dir().join(format!("bstd-upgrades-limits-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let quiet = crate::rt::open_native(
        blaze_core::testing::os(),
        crate::args::ArgList::given(Vec::new()),
        crate::env::Vars::given(Vec::new()),
        &dir,
        0,
    )
    .expect("quiet native world");
    assert_eq!(quiet.limits().trace_most, 0);

    let traced = crate::rt::open_native(
        blaze_core::testing::os(),
        crate::args::ArgList::given(Vec::new()),
        crate::env::Vars::given(vec![("BLAZE_TRACE".to_owned(), "1".to_owned())]),
        &dir,
        0,
    )
    .expect("traced native world");
    assert_eq!(traced.limits().trace_most, Limits::USUAL.trace_most);

    drop((quiet, traced));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn par_by_group_steers_shards_across_l3_clusters_and_inline() {
    use crate::task::threads::{Class, Plants, Topology};

    // 1. Inline world: `groups() == 1`, every shard runs once in order.
    let io = world(11, Plant::None);
    let cx = root(&io);
    let scope = cx.scope();
    assert_eq!(scope.groups(), 1);
    let mut shards = vec![0usize; 6];
    let res = par::by_group(&cx, &scope, &mut shards, 64 * 1024, |_wcx, idx, slot| {
        *slot = idx + 100;
    });
    let joined = scope.join(&cx);
    assert!(res.is_ok(), "inline by_group failed: {res:?}");
    assert!(joined.is_ok());
    assert_eq!(shards, vec![100, 101, 102, 103, 104, 105]);

    // 2. Multi-cluster pool (2 L3 groups x 2 workers = 4 workers): `scope.groups() == 2`.
    let topo = Topology::groups(&[(Class::Foreground, 2), (Class::Foreground, 2)]);
    let running = crate::rt::pool::start(&topo, Plants::NONE).expect("start 2-cluster pool");
    let frame = blaze_mem::Frame::with_reserve(TASK_FRAME, blaze_mem::Tag::Frames).expect("root frame");
    let deadline = blaze_core::wait::Deadline::after(io.now(), Duration::from_secs(10));
    let pcx = crate::Cx::root_on(deadline, frame, io.clone(), running.pool().clone());

    let pscope = pcx.scope();
    assert_eq!(pscope.groups(), 2);
    let mut pshards = vec![0usize; 16];
    let pres = par::by_group(&pcx, &pscope, &mut pshards, 64 * 1024, |wcx, idx, slot| {
        // Also exercise zero-atomic `Cx::span` / `Cx::until` from every cluster worker.
        let s = wcx.span("cluster.shard");
        let _u = s.until(deadline);
        *slot = idx * 3 + 1;
    });
    let pjoined = pscope.join(&pcx);
    drop(pcx);
    running.stop(Duration::from_secs(5)).expect("stop 2-cluster pool");

    assert!(pres.is_ok(), "multi-cluster by_group failed: {pres:?}");
    assert!(pjoined.is_ok());
    let expected: Vec<usize> = (0..16).map(|i| i * 3 + 1).collect();
    assert_eq!(pshards, expected);
}

#[test]
fn fs_read_all_many_and_vectored_writes_round_trip() {
    let io = world(12, Plant::None);
    let cx = root(&io);
    let dir = io.need_fs().expect("fs").open_dir(&cx, &path("data")).expect("dir");

    // 1. Seed 80 files (> WINDOW=64) via one txn, then read all 80 concurrently via `read_all_many`.
    let mut txn = dir.txn();
    let mut owned_paths = Vec::with_capacity(80);
    for i in 0..80usize {
        let p = path(&format!("f{i:03}"));
        let body = format!("payload-{i:03}");
        txn.publish(&cx, &p, Content::bytes(body.as_bytes())).expect("stage");
        owned_paths.push(p);
    }
    let committed = txn.commit_unsynced(&cx, Unsynced::No).expect("commit 80 files");
    assert_eq!(committed.published, 80);

    let refs: Vec<&crate::path::Path> = owned_paths.iter().collect();
    let mut bufs: Vec<Bytes<'_>> = (0..80).map(|_| cx.bytes()).collect();
    let total = dir.read_all_many(&cx, &refs, &mut bufs).expect("read_all_many 80 files");
    let mut expected_total = 0usize;
    for (i, buf) in bufs.iter().enumerate() {
        let want = format!("payload-{i:03}");
        assert_eq!(buf.as_slice(), want.as_bytes(), "mismatch at file {i}");
        expected_total += want.len();
    }
    assert_eq!(total, expected_total);

    // Missing file returns `FsKind::NotFound`.
    let missing = path("does_not_exist");
    let mut one_buf = [cx.bytes()];
    let err = dir.read_all_many(&cx, &[&missing], &mut one_buf).expect_err("missing file");
    assert_eq!(err.kind, FsKind::NotFound);

    // 2. `write_vectored_unsynced` and `append_vectored_unsynced` (0, 1, and multi-slice).
    let f = match dir.create(&cx, &path("vec.bin")) {
        Ok(f) => f,
        Err(e) => panic!("create: {e}"),
    };
    let r1 = f.write_vectored_unsynced(&cx, 0, &[b"", b""], Unsynced::No);
    let r2 = f.write_vectored_unsynced(&cx, 0, &[b"head-"], Unsynced::No);
    let r3 = f.write_vectored_unsynced(&cx, 5, &[b"mid-", b"", b"body-"], Unsynced::No);
    let r4 = f.append_vectored_unsynced(&cx, &[b"tail-", b"end"], Unsynced::No);
    let r5 = f.close_unsynced(Unsynced::No);
    r1.expect("empty vectored write");
    r2.expect("single vectored write");
    r3.expect("multi vectored write");
    r4.expect("multi vectored append");
    r5.expect("close");

    let mut got = cx.bytes();
    dir.read_all(&cx, &path("vec.bin"), &mut got).expect("read vec.bin");
    assert_eq!(got.as_slice(), b"head-mid-body-tail-end");
}

#[test]
fn native_readonly_inline_calls_and_quiet_spans_take_zero_world_locks() {
    let dir = std::env::temp_dir().join(format!("bstd-upgrades-lockfree-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join("hello.txt"), b"lock-free-read").expect("seed file");

    let io = crate::rt::open_native(
        blaze_core::testing::os(),
        crate::args::ArgList::given(Vec::new()),
        crate::env::Vars::given(Vec::new()),
        &dir,
        0,
    )
    .expect("quiet native io");
    let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
    let root_dir = io.need_fs().expect("fs").root();
    let seen = io.seen().expect("native world has seen counts");

    // Snapshot global `world` lock acquisitions BEFORE hot-path read-only ops + spans.
    let before = seen.locks.load(Ordering::Relaxed);
    let inline_before = seen.inline.load(Ordering::Relaxed);

    let p = path("hello.txt");
    for _ in 0..8 {
        let s = cx.span("hot.read");
        s.event("step");
        let meta = root_dir.stat(&s, &p).expect("stat").expect("exists");
        assert_eq!(meta.len, 14);
        let looks = root_dir.looks(&s, [&p]).expect("looks");
        assert_eq!(looks.len(), 1);
        let entries = root_dir.own_entries(&s).expect("entries");
        assert!(entries.find("hello.txt").is_some());
        let mut buf = s.bytes();
        root_dir.read_all(&s, &p, &mut buf).expect("read_all");
        assert_eq!(buf.as_slice(), b"lock-free-read");
        let f = root_dir.read_only().open(&s, &p).expect("open ro");
        let mut part = s.bytes();
        f.read_at(&s, 5, 4, &mut part).expect("read_at");
        assert_eq!(part.as_slice(), b"free");
    }

    let locks_taken = seen.locks.load(Ordering::Relaxed) - before;
    let inline_done = seen.inline.load(Ordering::Relaxed) - inline_before;
    drop((root_dir, cx, io));
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(locks_taken, 0, "native read-only inline calls + quiet spans must take 0 global world locks (took {locks_taken})");
    assert!(inline_done >= 40, "expected >= 40 inline read ops, got {inline_done}");
}

#[cfg(unix)]
#[test]
fn dir_read_all_confined_blocks_leaf_and_intermediate_symlink_escapes() {
    let base = std::env::temp_dir().join(format!("bstd-confined-{}", std::process::id()));
    let root_path = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(root_path.join("sub")).expect("root/sub");
    std::fs::create_dir_all(&outside).expect("outside");

    std::fs::write(root_path.join("sub/good.txt"), b"inside-ok").expect("good file");
    std::fs::write(outside.join("secret.txt"), b"TOP-SECRET").expect("outside secret");
    std::os::unix::fs::symlink(outside.join("secret.txt"), root_path.join("leaf_escape")).expect("leaf symlink");
    std::os::unix::fs::symlink(&outside, root_path.join("dir_escape")).expect("dir symlink");

    let io = crate::rt::open_native(
        blaze_core::testing::os(),
        crate::args::ArgList::given(Vec::new()),
        crate::env::Vars::given(Vec::new()),
        &root_path,
        0,
    )
    .expect("native io");
    let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
    let dir = io.need_fs().expect("fs").root();
    let seen = io.seen().expect("seen");

    let before = seen.locks.load(Ordering::Relaxed);
    let mut good = cx.bytes();
    let n = dir.read_all_confined(&cx, &path("sub/good.txt"), &mut good).expect("confined good read");
    assert_eq!(n, 9);
    assert_eq!(good.as_slice(), b"inside-ok");
    assert_eq!(seen.locks.load(Ordering::Relaxed) - before, 0, "confined native read must take 0 global world locks");

    // Both leaf symlink and intermediate directory symlink MUST be refused by the kernel (ELOOP).
    let mut leak1 = cx.bytes();
    assert!(dir.read_all_confined(&cx, &path("leaf_escape"), &mut leak1).is_err(), "leaf symlink escape must be refused");
    assert!(leak1.as_slice().is_empty());

    let mut leak2 = cx.bytes();
    assert!(dir.read_all_confined(&cx, &path("dir_escape/secret.txt"), &mut leak2).is_err(), "dir symlink escape must be refused");
    assert!(leak2.as_slice().is_empty());

    drop((good, leak1, leak2));
    drop((dir, cx, io));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn pending_wait_set_multiplexes_n_ops_and_conn_recv_skips_zero_fill() {
    use crate::pending::WaitSet;

    let io = world(13, Plant::None);
    let cx = root(&io);
    let dir = io.need_fs().expect("fs").open_dir(&cx, &path("data")).expect("dir");

    // 1. Seed 12 files, open them read-only, push 12 `File::read_into` ops into ONE `WaitSet`.
    let mut txn = dir.txn();
    let mut files = Vec::with_capacity(12);
    for i in 0..12usize {
        let p = path(&format!("w{i:02}"));
        let body = format!("waitset-item-{i:02}");
        txn.publish(&cx, &p, Content::bytes(body.as_bytes())).expect("stage");
    }
    txn.commit_unsynced(&cx, Unsynced::No).expect("commit");
    let ro = dir.read_only();
    for i in 0..12usize {
        files.push(ro.open(&cx, &path(&format!("w{i:02}"))).expect("open"));
    }
    let mut bufs: Vec<[u8; 32]> = vec![[0u8; 32]; 12];
    let mut got_len = vec![None::<usize>; 12];
    {
        let mut set = WaitSet::new(&cx);
        for (i, (file, buf)) in files.iter().zip(bufs.iter_mut()).enumerate() {
            if let Err((stopped, pending)) = set.push(i, file.read_into(buf, 0)) {
                let _ = pending.wait(&cx);
                panic!("waitset push failed: {stopped:?}");
            }
        }
        assert_eq!(set.len(), 12);
        // Pull first 4 via `next`, drain remaining 8 via `drain`.
        for _ in 0..4 {
            let (id, res) = set.next(&cx).expect("non-empty").expect("ok");
            got_len[id] = Some(res.expect("read ok"));
        }
        assert_eq!(set.len(), 8);
        set.drain(&cx, |id, res| {
            got_len[id] = Some(res.expect("drain read ok"));
        })
        .expect("drain succeeds");
    }
    for i in 0..12usize {
        let n = got_len[i].expect("every id completed");
        let want = format!("waitset-item-{i:02}");
        assert_eq!(&bufs[i][..n], want.as_bytes());
    }

    // 2. Verify `Conn::recv` (zero-memset path) round-trips with exact byte budget refund.
    let net = io.need_net().expect("net").limit(64);
    let addr = Addr::Tcp(HostPort::new("127.0.0.1", 9093).expect("addr"));
    let mut listener = match net.listen(&cx, &addr) {
        Ok(l) => l,
        Err(e) => panic!("listen: {e}"),
    };
    let client = match net.connect(&cx, &addr) {
        Ok(c) => c,
        Err(e) => panic!("connect: {e}"),
    };
    let server = match listener.accept(&cx) {
        Ok(s) => s,
        Err(e) => panic!("accept: {e}"),
    };
    client.send_vectored(&cx, &[b"hello-", b"recv"]).expect("send");
    let mut into = cx.bytes();
    let n = server.recv(&cx, 32, &mut into).expect("recv");
    assert_eq!(n, 10);
    assert_eq!(into.as_slice(), b"hello-recv");
    // 64 total budget - 10 sent - 10 received = 44 remaining (unused 22 refunded accurately!).
    assert_eq!(net.limit_left(), Some(44));
    let _ = client.finish(&cx);
    let _ = server.finish(&cx);
    let _ = listener.close(&cx);
}




