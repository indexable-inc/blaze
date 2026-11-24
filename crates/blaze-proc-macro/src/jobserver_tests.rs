//! The jobserver's promises: `workers - 1` tokens at rest in a 0600 fifo, a child that exits
//! holding tokens costs none (they come back by count once no run is running), and the fifo is
//! gone at the command's end.

use core::num::NonZeroU32;
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
};

use super::{FOLDER, Jobserver, JobserverOpts, Returned, TOKEN};

fn work(name: &str) -> std::string::String {
    let dir = std::env::temp_dir().join(format!("blaze-jobserver-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a test folder");
    dir.canonicalize().expect("its real path").to_str().expect("a UTF-8 temp path").to_owned()
}

/// A client as the `jobserver` crate opens `fifo:PATH`: read and write.
fn client(path: &str) -> std::fs::File {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).expect("the fifo opens as a client")
}

/// Takes up to `most` tokens without blocking.
fn take(file: &mut std::fs::File, most: usize) -> usize {
    let mut got = 0;
    let mut byte = [0u8; 1];
    while got < most {
        match file.read(&mut byte) {
            Ok(1) => {
                assert_eq!(byte[0], TOKEN);
                got += 1;
            }
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("{error}"),
        }
    }
    got
}

/// Four workers: three tokens at rest, fifo mode 0600 in a 0700 folder. A child that takes two
/// and exits keeping them (killed) gives them back by count when the run ends; one that returns
/// what it took gives back nothing more. While another run is still running nothing is counted
/// (its child may hold tokens). Control: without the give-back the second child would find one
/// token, not three (asserted by taking before the end). At the end the fifo is gone.
/// Prints "jobserver: 3 tokens at rest, 2 kept by a killed child returned by count, fifo removed".
#[test]
fn jobserver_returns_a_dead_childs_tokens_by_count_and_is_its_owners_alone() {
    let work = work("count");
    let server = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::new(4).expect("4"), id: 7 }).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(server.fifo(), format!("{work}/{FOLDER}/7"));
    assert_eq!(server.tokens(), 3);
    let meta = std::fs::metadata(server.fifo()).expect("the fifo");
    assert!(meta.file_type().is_fifo());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(format!("{work}/{FOLDER}")).expect("its folder").permissions().mode() & 0o777, 0o700);

    // A run whose child takes two tokens and is killed holding them.
    let lent = server.lend().unwrap_or_else(|e| panic!("{e}"));
    let other = server.lend().unwrap_or_else(|e| panic!("{e}"));
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 2), 2);
    }
    // Another run is still running: nothing is counted yet.
    assert_eq!(lent.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 0 });
    {
        let mut peek = client(server.fifo());
        let left = take(&mut peek, 8);
        assert_eq!(left, 1, "the control: one token left while the killed child's two are out");
        peek.write_all(&[TOKEN]).expect("the peeked token back");
    }
    // The last run ends: the two come back by count.
    assert_eq!(other.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 2 });
    assert_eq!(server.restored().unwrap_or_else(|e| panic!("{e}")), 2);
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 8), 3, "all three tokens at rest again");
        child.write_all(&[TOKEN; 3]).expect("returned");
    }
    // A child that gives back what it took: nothing is restored.
    let lent = server.lend().unwrap_or_else(|e| panic!("{e}"));
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 3), 3);
        child.write_all(&[TOKEN; 3]).expect("returned");
    }
    assert_eq!(lent.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 0 });
    assert_eq!(server.restored().unwrap_or_else(|e| panic!("{e}")), 2);

    server.end().unwrap_or_else(|e| panic!("{e}"));
    assert!(std::fs::metadata(server.fifo()).is_err(), "the fifo is removed at the end");
    assert!(server.lend().is_err(), "no run is lent after the end");
    println!("jobserver: 3 tokens at rest, 2 kept by a killed child returned by count, fifo removed");
    _ = std::fs::remove_dir_all(&work);
}

/// A stale fifo of a dead command with the same id is replaced, not reused (its tokens are
/// unknown); one worker gives no tokens at all (the script's implicit token is its worker).
#[test]
fn jobserver_replaces_a_stale_fifo_and_one_worker_lends_nothing() {
    let work = work("stale");
    let first = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::new(3).expect("3"), id: 9 }).unwrap_or_else(|e| panic!("{e}"));
    let path = first.fifo().to_owned();
    // The dead command never ended its jobserver; a new one of the same id starts afresh.
    let second = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::MIN, id: 9 }).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second.fifo(), path);
    assert_eq!(second.tokens(), 0);
    let mut child = client(second.fifo());
    assert_eq!(take(&mut child, 8), 0);
    second.end().unwrap_or_else(|e| panic!("{e}"));
    drop(first);
    _ = std::fs::remove_dir_all(&work);
}

/// Overlapping native build-script runs of the same `key_hex` (either two threads of one command
/// or two concurrent commands sharing `<home>/.blaze/v2/native-work/blaze`) get disjoint `(id, seq)`
/// work directories from [`Jobserver::next_run_id`], so neither run's `fresh_folder` deletes files
/// out from under the other's `OutDir::list_output` (`FilesError { op: ListOutput, code: 2 }`),
/// and `RunWorkGuard` removes all private directories and lock files on drop.
#[test]
fn overlapping_runs_of_same_key_use_disjoint_work_dirs_and_clean_up() {
    use bstd::{fs::Durable, mem::TASK_FRAME, procs::hermetic::RootFiles as _};

    let alloc = blaze_own::testing::heap().alloc();
    let work_dir = work("run-isolation");
    let cmd_a = Jobserver::new(JobserverOpts { work: &work_dir, workers: NonZeroU32::new(2).expect("2"), id: 101 }).expect("cmd_a");
    let cmd_b = Jobserver::new(JobserverOpts { work: &work_dir, workers: NonZeroU32::new(2).expect("2"), id: 202 }).expect("cmd_b");

    let (io, root) = crate::record::native_world(work_dir.as_ref()).expect("native world");
    let cx = io.cx(core::time::Duration::from_secs(60), TASK_FRAME).expect("cx");
    let shim_rel = bstd::path::Path::new("fake-shim").expect("shim path");
    let sf = root.create(&cx, &shim_rel).expect("create fake shim");
    sf.write_at(&cx, 0, b"#!/bin/sh\nexit 0\n", Durable::No).expect("write shim");
    sf.close(&cx, Durable::No).expect("close shim");
    let shim_path = blaze_own::format_in!(alloc, "{work_dir}/fake-shim");
    let placed = crate::native_facts::place_shim(&work_dir, shim_path.as_str()).expect("place shim");

    let same_key = "1f7948020d269c853f239f31e1e4e69149b7929842b80f36f305779778cd3eff";
    let (a0, a1, b0) = (cmd_a.next_run_id(), cmd_a.next_run_id(), cmd_b.next_run_id());
    assert_eq!(a0, (101, 0));
    assert_eq!(a1, (101, 1));
    assert_eq!(b0, (202, 0));

    let rk_a0 = blaze_own::format_in!(alloc, "{same_key}-{}-{}", a0.0, a0.1);
    let rk_a1 = blaze_own::format_in!(alloc, "{same_key}-{}-{}", a1.0, a1.1);
    let rk_b0 = blaze_own::format_in!(alloc, "{same_key}-{}-{}", b0.0, b0.1);

    let out_a0_rel = blaze_own::format_in!(alloc, "out/{}", rk_a0.as_str());
    let out_a1_rel = blaze_own::format_in!(alloc, "out/{}", rk_a1.as_str());
    let out_b0_rel = blaze_own::format_in!(alloc, "out/{}", rk_b0.as_str());
    let lock_a0_rel = blaze_own::format_in!(alloc, "facts/{}.lock", rk_a0.as_str());
    {
        let mut guard_a0 = crate::native_unit::RunWorkGuard::new(&work_dir, alloc);
        let out_a0 = crate::native_unit::fresh_folder(&work_dir, out_a0_rel.as_str(), &[], "tikv-jemalloc-sys", alloc).expect("out a0");
        guard_a0.push_dir(blaze_own::AString::from_str_in(out_a0_rel.as_str(), alloc));
        let _facts_a0 = crate::native_facts::prepare(&placed, &work_dir, rk_a0.as_str(), None, alloc).expect("facts a0");
        guard_a0.push_dir(blaze_own::format_in!(alloc, "facts/{}", rk_a0.as_str()));
        guard_a0.push_file(blaze_own::AString::from_str_in(lock_a0_rel.as_str(), alloc));

        // Write a nested file in run a0, then start overlapping runs a1 and b0 for the SAME key_hex.
        let nested_rel = blaze_own::format_in!(alloc, "{}/build/lib/libjemalloc.a", out_a0_rel.as_str());
        let nf = root.create(&cx, &bstd::path::Path::new(nested_rel.as_str()).expect("nested")).expect("create a0 output");
        nf.write_at(&cx, 0, b"archive-a0", Durable::No).expect("write a0");
        nf.close(&cx, Durable::No).expect("close a0");

        let mut guard_a1 = crate::native_unit::RunWorkGuard::new(&work_dir, alloc);
        let _out_a1 = crate::native_unit::fresh_folder(&work_dir, out_a1_rel.as_str(), &[], "tikv-jemalloc-sys", alloc).expect("out a1");
        guard_a1.push_dir(blaze_own::AString::from_str_in(out_a1_rel.as_str(), alloc));
        let _facts_a1 = crate::native_facts::prepare(&placed, &work_dir, rk_a1.as_str(), None, alloc).expect("facts a1");
        guard_a1.push_dir(blaze_own::format_in!(alloc, "facts/{}", rk_a1.as_str()));
        guard_a1.push_file(blaze_own::format_in!(alloc, "facts/{}.lock", rk_a1.as_str()));

        let mut guard_b0 = crate::native_unit::RunWorkGuard::new(&work_dir, alloc);
        let _out_b0 = crate::native_unit::fresh_folder(&work_dir, out_b0_rel.as_str(), &[], "tikv-jemalloc-sys", alloc).expect("out b0");
        guard_b0.push_dir(blaze_own::AString::from_str_in(out_b0_rel.as_str(), alloc));
        let _facts_b0 = crate::native_facts::prepare(&placed, &work_dir, rk_b0.as_str(), None, alloc).expect("facts b0");
        guard_b0.push_dir(blaze_own::format_in!(alloc, "facts/{}", rk_b0.as_str()));
        guard_b0.push_file(blaze_own::format_in!(alloc, "facts/{}.lock", rk_b0.as_str()));

        // Run a0's `list_output` must still see its file cleanly (never ENOENT from a1/b0).
        let out = crate::native_script::OutDir { root: blaze_roots::RootId(7), folder: &out_a0, package_root: None };
        let mut count = 0usize;
        out.list_output(&mut |_path, _digest, len| {
            assert_eq!(len, 10);
            count += 1;
            Ok(())
        })
        .expect("list_output succeeds despite overlapping runs of the same key");
        assert_eq!(count, 1);
    }

    // All private directories and lock files were cleaned up on drop.
    assert!(root.stat(&cx, &bstd::path::Path::new(out_a0_rel.as_str()).expect("p")).expect("stat").is_none());
    assert!(root.stat(&cx, &bstd::path::Path::new(out_a1_rel.as_str()).expect("p")).expect("stat").is_none());
    assert!(root.stat(&cx, &bstd::path::Path::new(out_b0_rel.as_str()).expect("p")).expect("stat").is_none());
    assert!(root.stat(&cx, &bstd::path::Path::new(lock_a0_rel.as_str()).expect("p")).expect("stat").is_none());

    cmd_a.end().expect("end cmd_a");
    cmd_b.end().expect("end cmd_b");
    let _ = crate::native_script::remove(&work_dir, "fake-shim");
}

/// Placing two different `blaze-fact-shim` binaries into the same shared native work directory
/// puts them at distinct content-addressed paths under `fact-shim/<digest>/blaze-fact-shim`, so a
/// concurrent or rebuilt `blaze` binary never overwrites the shim an in-flight command is running
/// scripts against (which otherwise causes `EngineError::TwoValues` on `Name { kind: Given, .. }`).
#[test]
fn place_shim_content_addresses_distinct_shims_without_clobbering() {
    use bstd::{fs::Durable, mem::TASK_FRAME, procs::hermetic::RootFiles as _};

    let alloc = blaze_own::testing::heap().alloc();
    let work_dir = work("shim-content-addr");
    let (io, root) = crate::record::native_world(work_dir.as_ref()).expect("native world");
    let cx = io.cx(core::time::Duration::from_secs(60), TASK_FRAME).expect("cx");

    let write_shim = |name: &str, bytes: &[u8]| {
        let rel = bstd::path::Path::new(name).expect("rel");
        let f = root.create(&cx, &rel).expect("create");
        f.write_at(&cx, 0, bytes, Durable::No).expect("write");
        f.close(&cx, Durable::No).expect("close");
        blaze_own::format_in!(alloc, "{work_dir}/{name}")
    };

    let bytes_a = b"#!/bin/sh\necho shim-a\n";
    let bytes_b = b"#!/bin/sh\necho shim-b\n";
    let src_a = write_shim("fake-shim-a", bytes_a);
    let src_b = write_shim("fake-shim-b", bytes_b);

    let placed_a = crate::native_facts::place_shim(&work_dir, src_a.as_str()).expect("place shim a");
    let placed_b = crate::native_facts::place_shim(&work_dir, src_b.as_str()).expect("place shim b");
    let placed_a2 = crate::native_facts::place_shim(&work_dir, src_a.as_str()).expect("place shim a again");

    assert_ne!(placed_a.as_str(), placed_b.as_str(), "distinct shim contents must get distinct paths");
    assert_eq!(placed_a.as_str(), placed_a2.as_str(), "identical shim contents reuse the same content-addressed path");
    let read_placed = |path: &str| {
        let mut buf = [0u8; 64];
        let mut f = OpenOptions::new().read(true).open(path).expect("open placed");
        let n = f.read(&mut buf).expect("read placed");
        (buf, n)
    };
    let (buf_a, len_a) = read_placed(placed_a.as_str());
    let (buf_b, len_b) = read_placed(placed_b.as_str());
    assert_eq!(buf_a.get(..len_a), Some(bytes_a.as_slice()), "placing b must not clobber a");
    assert_eq!(buf_b.get(..len_b), Some(bytes_b.as_slice()), "placed b has its own bytes");

    let _ = crate::native_script::remove(&work_dir, "fake-shim-a");
    let _ = crate::native_script::remove(&work_dir, "fake-shim-b");
}
