//! "A cost or a guarantee the caller should decide is an argument or a type, never a default"
//! (docs/v2/plan/design/std.md, "The second rule"): one example test for each place bstd used
//! to decide for its caller, showing that the explicit form does what it says and that what
//! was implicit is gone. The invariant across crashes is [`super::durable`]'s.
//!
//! What only a compiler can show is not here. That a `File<ReadWrite>` or a `Buffered` cannot
//! be dropped unfinished is in crates/hir-ty-linear's fixtures, which mirror bstd's linear
//! types (`bstd_a_writable_file_left_unclosed_is_an_error`,
//! `bstd_a_buffered_writer_left_unfinished_is_an_error`). That `Written` has no `clone_to`,
//! and that `Dir::write`, `File::sync`, `File::replace`, `Conn::send` and a two-argument
//! `Scope::spawn` no longer exist, has no harness: the crate has no compile-fail tests, and
//! the spec lists the fixture that is owed.

use std::{num::NonZero, time::Duration};

use super::{path, root, world, write_synced};
use crate::{
    Plant, Stopped,
    blobs::{Blobs, BlobsKind},
    env::VarName,
    fs::{Content, Dir, Durable, File, FsKind, FsOp, How, Placed, Pushed, ReadWrite, Unsynced},
    io::{Io, SimWorld},
    kv::{Key, Kv},
    local::{Pair, pair},
    mem::TASK_FRAME,
    trace::RecordKind,
};

fn data(io: &Io) -> (crate::Cx, Dir<ReadWrite>) {
    let cx = root(io);
    let dir = io.need_fs().expect("a file tree").open_dir(&cx, &path("data")).expect("a directory");
    (cx, dir)
}

/// The file `at` made empty. A writable file is linear, so it is taken out of its `Result`
/// by a match, never through a generic `expect`.
fn create(cx: &crate::Cx, dir: &Dir<ReadWrite>, at: &str) -> File<ReadWrite> {
    match dir.create(cx, &path(at)) {
        Ok(file) => file,
        Err(error) => panic!("a create of {at}: {error}"),
    }
}

/// What `at` holds, or `None`.
fn held(cx: &crate::Cx, dir: &Dir<ReadWrite>, at: &str) -> Option<Vec<u8>> {
    let mut into = cx.bytes();
    match dir.read_all(cx, &path(at), &mut into) {
        Ok(_) => Some(into.as_slice().to_vec()),
        Err(error) if error.kind == FsKind::NotFound => None,
        Err(error) => panic!("a read of {at}: {error}"),
    }
}

#[test]
fn an_unsynced_publish_is_lost_by_a_crash_and_a_synced_one_is_not() {
    let (kept, lost): (&[u8], &[u8]) = (b"kept through the crash", b"derived, made again");
    let mut seeds_that_lost = 0;
    for seed in 0..64 {
        let io = world(seed, Plant::None);
        let (cx, dir) = data(&io);
        dir.publish_synced(&cx, &path("kept"), Content::bytes(kept)).expect("a synced publish");
        dir.publish_unsynced(&cx, &path("lost"), Content::bytes(lost), Unsynced::No).expect("an unsynced publish");
        // Before a crash both read whole.
        assert_eq!(held(&cx, &dir, "kept").as_deref(), Some(kept));
        assert_eq!(held(&cx, &dir, "lost").as_deref(), Some(lost));
        assert!(io.crash());
        assert_eq!(held(&cx, &dir, "kept").as_deref(), Some(kept), "seed {seed}: a synced publish survives");
        match held(&cx, &dir, "lost") {
            None => seeds_that_lost += 1,
            Some(part) => {
                assert!(lost.starts_with(&part), "seed {seed}: an unsynced publish is lost or cut short, never other bytes");
                if part.len() < lost.len() {
                    seeds_that_lost += 1;
                }
            }
        }
    }
    assert!(seeds_that_lost > 0, "no crash of 64 lost an unsynced publish: Durable::No is being synced");
}

#[test]
fn an_unsynced_remove_may_come_back_and_a_synced_one_does_not() {
    let mut came_back = 0;
    for seed in 0..64 {
        let io = world(seed, Plant::None);
        let (cx, dir) = data(&io);
        write_synced(&cx, &dir, &path("gone"), b"one").expect("a file");
        write_synced(&cx, &dir, &path("maybe"), b"two").expect("a file");
        dir.remove_synced(&cx, &path("gone")).expect("a synced remove");
        dir.remove(&cx, &path("maybe"), Durable::No).expect("an unsynced remove");
        assert_eq!((held(&cx, &dir, "gone"), held(&cx, &dir, "maybe")), (None, None));
        assert!(io.crash());
        assert_eq!(held(&cx, &dir, "gone"), None, "seed {seed}: a synced remove stays gone");
        match held(&cx, &dir, "maybe") {
            None => {}
            Some(back) => {
                assert_eq!(back, b"two", "seed {seed}: what comes back is what was there");
                came_back += 1;
            }
        }
    }
    assert!(came_back > 0, "no crash of 64 undid an unsynced remove: Durable::No is being synced");
}

#[test]
fn no_unsynced_call_waits_for_the_device_and_each_synced_one_does() {
    let io = world(7, Plant::None);
    let (cx, dir) = data(&io);
    let waits = |io: &Io| io.disk().expect("a simulated disk").device_waits;
    dir.publish_unsynced(&cx, &path("a"), Content::bytes(b"aaaa"), Unsynced::No).expect("a publish");
    dir.rename_replacing_unsynced(&cx, &path("a"), &path("b"), Unsynced::No).expect("a rename");
    let file = create(&cx, &dir, "log");
    file.append_unsynced(&cx, b"0123456789", Unsynced::No).expect("an append");
    file.write_at_unsynced(&cx, 2, b"xx", Unsynced::No).expect("a write");
    file.set_len_unsynced(&cx, 6, Unsynced::No).expect("a cut");
    let written = file.close_unsynced(Unsynced::No).expect("a close");
    assert_eq!(held(&cx, &dir, "log").as_deref(), Some(&b"01xx45"[..]));
    dir.remove_unsynced(&cx, &path("b"), Unsynced::No).expect("a remove");
    let kv = Kv::beneath(&dir.subdir(&path("kv")));
    kv.put_unsynced(&cx, &Key::new("k").expect("a key"), b"v", Unsynced::No).expect("a put");
    kv.delete_unsynced(&cx, &Key::new("k").expect("a key"), Unsynced::No).expect("a delete");
    let blobs = Blobs::beneath(&dir.subdir(&path("blobs")));
    let id = blobs.put_unsynced(&cx, b"blob", Unsynced::No).expect("a put");
    blobs.delete_unsynced(&cx, &id, Unsynced::No).expect("a delete");
    let counts = io.disk().expect("a simulated disk");
    assert_eq!((counts.device_waits, counts.flushes, counts.name_syncs), (0, 0, 0), "an _unsynced call asked the device");
    // Each synced form waits: a flush is one wait, a publish two (its bytes, then its name).
    written.flush(&cx).expect("a flush");
    assert_eq!(waits(&io), 1);
    dir.publish_synced(&cx, &path("c"), Content::bytes(b"cccc")).expect("a publish");
    assert_eq!(waits(&io), 3);
    dir.remove_synced(&cx, &path("c")).expect("a remove");
    assert_eq!(waits(&io), 4);
    dir.sync_names(&cx).expect("a name sync");
    assert_eq!(waits(&io), 5);
}

#[test]
fn started_is_refused_before_anything_changes() {
    let io = world(8, Plant::None);
    let (cx, dir) = data(&io);
    write_synced(&cx, &dir, &path("f"), b"old").expect("a file");
    let before = io.disk().expect("a simulated disk");
    let refused = dir.publish(&cx, &path("f"), Content::bytes(b"new"), Durable::Started).expect_err("no driver starts writeback");
    assert_eq!((refused.op, &refused.kind), (FsOp::Publish, &FsKind::NoWritebackStart));
    assert!(refused.to_string().contains("Durable::Started"), "{refused}");
    let refused = dir.remove(&cx, &path("f"), Durable::Started).expect_err("no driver starts writeback");
    assert_eq!(refused.kind, FsKind::NoWritebackStart);
    assert_eq!(held(&cx, &dir, "f").as_deref(), Some(&b"old"[..]));
    let after = io.disk().expect("a simulated disk");
    assert_eq!((before.written, before.flushes, before.device_waits), (after.written, after.flushes, after.device_waits));
}

#[test]
fn a_clone_of_a_flushed_file_forces_nothing_and_one_of_unflushed_bytes_is_charged() {
    let io = world(9, Plant::None);
    let (cx, dir) = data(&io);
    let flushed = dir.publish_synced(&cx, &path("base"), Content::bytes(&[7; 100])).expect("a publish").file;
    flushed.clone_to(&cx, &dir, &path("clone")).expect("a clone of a flushed file");
    let counts = io.disk().expect("a simulated disk");
    assert_eq!((counts.clones, counts.forced_writeback), (1, 0));
    assert_eq!(held(&cx, &dir, "clone").as_deref(), Some(&[7u8; 100][..]));
    // What no type sees: the path is written again behind the `Flushed`, unsynced. The
    // simulation charges the clone for every byte it makes the kernel write out.
    dir.publish_unsynced(&cx, &path("base"), Content::bytes(&[8; 60]), Unsynced::No).expect("a publish");
    flushed.clone_to(&cx, &dir, &path("stale")).expect("a clone");
    let counts = io.disk().expect("a simulated disk");
    assert_eq!((counts.clones, counts.forced_writeback), (2, 60));
    // A copy of the same unflushed file forces nothing: it reads and writes.
    dir.publish_unsynced(&cx, &path("base"), Content::bytes(&[9; 40]), Unsynced::No).expect("a publish");
    let chunk = NonZero::new(16).expect("a chunk");
    let unflushed = dir.open_read(&cx, &path("base")).expect("the file");
    let copied = dir.publish_unsynced(&cx, &path("copy"), Content::copy_of(&unflushed, chunk), Unsynced::No).expect("a copy");
    assert_eq!(copied.how, How::Copied);
    assert_eq!(io.disk().expect("a simulated disk").forced_writeback, 60);
    assert_eq!(held(&cx, &dir, "copy").as_deref(), Some(&[9u8; 40][..]));
}

#[test]
fn a_publish_says_how_it_was_made_in_its_result_and_its_span() {
    let io = world(10, Plant::None);
    let (cx, dir) = data(&io);
    let cx = cx.span("place an output");
    let base = dir.publish_synced(&cx, &path("base"), Content::bytes(b"0123456789")).expect("a publish");
    assert_eq!(base.how, How::Written);
    let cloned = dir.publish_synced(&cx, &path("same"), Content::file(&base.file)).expect("a clone");
    assert_eq!(cloned.how, How::Cloned);
    let patches = [crate::fs::Patch { at: 2, bytes: b"xx" }, crate::fs::Patch { at: 9, bytes: b"yz" }];
    let derived = dir.publish_synced(&cx, &path("near"), Content::patch_of(&base.file, &patches)).expect("a derive");
    assert_eq!(derived.how, How::Derived);
    assert_eq!(held(&cx, &dir, "same").as_deref(), Some(&b"0123456789"[..]));
    assert_eq!(held(&cx, &dir, "near").as_deref(), Some(&b"01xx45678yz"[..]));
    let said: Vec<&'static str> = io.trace(|trace| {
        trace
            .records()
            .iter()
            .filter_map(|it| match (it.span, it.kind) {
                ("place an output", RecordKind::Event(what)) => Some(what),
                _ => None,
            })
            .collect()
    });
    assert_eq!(said, [How::Written.says(), How::Cloned.says(), How::Derived.says()]);
}

#[test]
fn clone_to_and_copy_to_make_a_new_file_and_never_replace_one() {
    let io = world(11, Plant::None);
    let (cx, dir) = data(&io);
    let flushed = dir.publish_synced(&cx, &path("base"), Content::bytes(b"base")).expect("a publish").file;
    write_synced(&cx, &dir, &path("taken"), b"mine").expect("a file");
    let refused = flushed.clone_to(&cx, &dir, &path("taken")).expect_err("a clone never replaces");
    assert_eq!((refused.op, refused.kind), (FsOp::CloneTo, FsKind::Exists));
    let chunk = NonZero::new(3).expect("a chunk");
    let refused = flushed.copy_to(&cx, &dir, &path("taken"), chunk).expect_err("a copy never replaces");
    assert_eq!((refused.op, refused.kind), (FsOp::CopyTo, FsKind::Exists));
    assert_eq!(held(&cx, &dir, "taken").as_deref(), Some(&b"mine"[..]));
    // A copy gives a file that is not flushed: its type has no `clone_to` until it is.
    let copied = flushed.copy_to(&cx, &dir, &path("copy"), chunk).expect("a copy");
    assert_eq!(held(&cx, &dir, "copy").as_deref(), Some(&b"base"[..]));
    let now_flushed = copied.flush(&cx).expect("a flush");
    now_flushed.clone_to(&cx, &dir, &path("clone-of-copy")).expect("a clone, once flushed");
    assert_eq!(io.disk().expect("a simulated disk").forced_writeback, 0);
}

#[test]
fn closing_a_file_gives_the_type_its_durability_made() {
    let io = world(12, Plant::None);
    let (cx, dir) = data(&io);
    let file = create(&cx, &dir, "no");
    let closed = file.close(&cx, Durable::No).expect("a close");
    assert!(matches!(closed, Placed::Written(_)), "{closed:?}");
    let file = create(&cx, &dir, "yes");
    file.append(&cx, b"bytes", Durable::No).expect("an append");
    let closed = file.close(&cx, Durable::Yes).expect("a close");
    assert!(matches!(closed, Placed::Flushed(_)), "{closed:?}");
    // The synced close made the file's name durable too; the unsynced one's may be undone.
    assert!(io.crash());
    assert_eq!(held(&cx, &dir, "yes").as_deref(), Some(&b"bytes"[..]));
    // A close told `Started` is refused and still gives the handle up.
    let file = create(&cx, &dir, "started");
    let refused = file.close(&cx, Durable::Started).expect_err("no driver starts writeback");
    assert_eq!((refused.op, refused.kind), (FsOp::Close, FsKind::NoWritebackStart));
}

#[test]
fn a_blob_torn_by_a_crash_is_refused_not_handed_back() {
    let mut torn = 0;
    for seed in 0..64 {
        let io = world(seed, Plant::None);
        let (cx, dir) = data(&io);
        let blobs = Blobs::beneath(&dir);
        let id = blobs.put_unsynced(&cx, &[5; 200], Unsynced::No).expect("a put");
        assert!(io.crash());
        let mut into = cx.bytes();
        match blobs.get(&cx, &id, &mut into) {
            Ok(None) => {}
            Ok(Some(read)) => assert_eq!((read, into.as_slice()), (200, &[5u8; 200][..]), "seed {seed}"),
            Err(error) => {
                assert!(matches!(error.kind, BlobsKind::Corrupt { .. }), "seed {seed}: {error}");
                assert!(into.is_empty(), "seed {seed}: bytes of a torn blob were handed back");
                torn += 1;
            }
        }
    }
    assert!(torn > 0, "no crash of 64 tore an unsynced blob");
}

#[test]
fn an_env_says_not_allowed_apart_from_not_set() {
    let mut sim = SimWorld::new(13);
    sim.env = vec![("HOME".to_owned(), "/home/sim".to_owned()), ("SECRET".to_owned(), "s3".to_owned())];
    let io = Io::simulated(sim);
    let name = |raw: &str| VarName::new(raw).expect("a test's variable");
    let env = io.need_env().expect("an environment").only(&[name("HOME"), name("UNSET")]);
    assert_eq!(env.get(&name("HOME")), Ok(Some("/home/sim")));
    assert_eq!(env.get(&name("UNSET")), Ok(None));
    let refused = env.get(&name("SECRET")).expect_err("narrowed away");
    assert_eq!(refused.name, name("SECRET"));
    assert!(refused.to_string().contains("SECRET"), "{refused}");
}

#[test]
fn a_task_gets_the_scratch_its_spawn_names() {
    let io = world(14, Plant::None);
    let cx = root(&io);
    let scope = cx.scope();
    let small = scope.spawn("small", 64 << 10, |cx| -> Result<(), Stopped> { cx.bytes().try_extend(&vec![0; 8 << 20]) });
    let roomy = scope.spawn("roomy", TASK_FRAME, |cx| -> Result<(), Stopped> { cx.bytes().try_extend(&vec![0; 1 << 20]) });
    let (small, roomy) = (small.join(&cx), roomy.join(&cx));
    let joined = scope.join(&cx);
    assert!(roomy.is_ok(), "{roomy:?}");
    let failed = small.expect_err("8 MiB do not fit a frame told 64 KiB");
    assert!(failed.to_string().contains("more bytes would pass its limit"), "{failed}");
    assert!(joined.is_err(), "the scope fails with its task");
    // The root's frame is said too.
    let tight = io.cx(Duration::from_secs(1), 64 << 10).expect("a root context");
    assert!(matches!(tight.bytes().try_extend(&vec![0; 8 << 20]), Err(Stopped::OutOfMemory { .. })));
}

#[test]
fn a_local_pair_has_the_capacity_it_was_given_and_write_all_says_how_far_it_got() {
    let io = world(15, Plant::None);
    let cx = root(&io);
    let clock = io.need_clock().expect("a clock");
    let Pair { first, second } = pair("explicit", NonZero::new(4).expect("a capacity"));
    let short = cx.until(clock.after(Duration::from_millis(1)));
    let stopped = first.write_all(&short, b"0123456").expect_err("seven bytes do not fit four");
    assert_eq!(stopped.moved, 4);
    let mut into = cx.bytes();
    assert_eq!(second.read(&cx, 16, &mut into).expect("a read"), 4);
    assert_eq!(into.as_slice(), b"0123");
    first.finish(&cx);
    second.finish(&cx);
}

#[test]
fn a_buffered_writer_holds_bytes_until_it_is_full_or_finished_and_says_which() {
    let io = world(16, Plant::None);
    let (cx, dir) = data(&io);
    let written = |io: &Io| io.disk().expect("a simulated disk").written;
    let file = create(&cx, &dir, "log");
    // The writer is linear: everything it is asked is gathered, then it is finished, then
    // the file is closed, and only then is anything asserted.
    let mut writer = match file.buffered(cx.frame(), NonZero::new(8).expect("a capacity")) {
        Ok(writer) => writer,
        Err(error) => panic!("a buffer of 8 bytes: {error}"),
    };
    let first = (writer.push(&cx, b"abc"), written(&io), writer.held());
    let second = (writer.push(&cx, b"def"), written(&io), writer.held());
    // Six held, three more do not fit eight: the six are written, the three kept.
    let third = (writer.push(&cx, b"ghi"), written(&io), writer.held());
    // As long as the buffer: what is held is written, then these, and nothing is kept.
    let fourth = (writer.push(&cx, b"0123456789"), written(&io), writer.held());
    let fifth = (writer.push(&cx, b"xy"), written(&io), writer.held());
    let before_finish = held(&cx, &dir, "log");
    let finished = writer.finish(&cx);
    let after_finish = held(&cx, &dir, "log");
    let closed = file.close_unsynced(Unsynced::No);
    assert_eq!(first, (Ok(Pushed::Held), 0, 3));
    assert_eq!(second, (Ok(Pushed::Held), 0, 6));
    assert_eq!(third, (Ok(Pushed::Wrote(6)), 6, 3));
    assert_eq!(fourth, (Ok(Pushed::Wrote(13)), 19, 0));
    assert_eq!(fifth, (Ok(Pushed::Held), 19, 2));
    assert_eq!(before_finish.as_deref(), Some(&b"abcdefghi0123456789"[..]), "held bytes are not in the file");
    assert_eq!(finished, Ok(2));
    assert_eq!(after_finish.as_deref(), Some(&b"abcdefghi0123456789xy"[..]));
    assert!(closed.is_ok());
    // An append with no buffer is in the file when its call returns.
    assert_eq!(io.disk().expect("a simulated disk").device_waits, 0);
}
