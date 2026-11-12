//! "A cost or a guarantee the caller should decide is an argument or a type, never a default"
//! (docs/v2/plan/design/std.md, "The second rule"): one example test for each place bstd used
//! to decide for its caller, showing that the explicit form does what it says and that what
//! was implicit is gone. The invariant across crashes is [`super::durable`]'s.
//!
//! What only a compiler can show is not here: it is the compile-fail harness
//! `bstd_the_wrong_call_does_not_compile` (crates/blaze-engine/tests/repro.rs), whose fixtures
//! are each one call bstd's types refuse (a clone from a `Written`, a `Buffered` dropped, a
//! write through a narrowed handle, a removed name) beside a control that checks.
//! crates/hir-ty-linear's fixtures mirror bstd's linear types for the check's own tests.

use std::{num::NonZero, time::Duration};

use super::{path, root, world, write_synced};
use crate::{
    Plant, Stopped,
    blobs::{Blobs, BlobsKind},
    env::VarName,
    fs::{Content, Dir, Durable, File, FsKind, FsOp, How, Placed, Pushed, ReadWrite, Unsynced},
    io::{Io, Limits, SimWorld},
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
fn started_begins_the_writeback_and_waits_for_nothing() {
    let io = world(8, Plant::None);
    let (cx, dir) = data(&io);
    write_synced(&cx, &dir, &path("f"), b"old").expect("a file");
    let before = io.disk().expect("a simulated disk");
    // `Started` makes the change, begins the write-back and waits for nothing: no flush is
    // asked, and no wait for the device is paid.
    let published = dir.publish(&cx, &path("f"), Content::bytes(b"new"), Durable::Started).expect("a publish");
    assert!(matches!(published.file, Placed::Written(_)), "Started gives no Flushed: {:?}", published.file);
    assert_eq!(held(&cx, &dir, "f").as_deref(), Some(&b"new"[..]));
    let after = io.disk().expect("a simulated disk");
    assert_eq!((after.writebacks - before.writebacks, after.flushes, after.device_waits), (1, before.flushes, before.device_waits));
    // A file whose write-back has ended is clean: flushed then, its clone forces nothing.
    let file = match dir.open(&cx, &path("f")) {
        Ok(file) => file,
        Err(error) => panic!("the file: {error}"),
    };
    let appended = file.append(&cx, &[1; 50], Durable::Started);
    let closed = file.close_unsynced(Unsynced::Started);
    appended.expect("an append");
    let flushed = closed.expect("a close").flush(&cx).expect("a flush");
    flushed.clone_to(&cx, &dir, &path("clone")).expect("a clone");
    assert_eq!(io.disk().expect("a simulated disk").forced_writeback, 0);
    // A remove told `Started` removes, and begins its directory's sync without waiting.
    let waits = io.disk().expect("a simulated disk").device_waits;
    dir.remove(&cx, &path("f"), Durable::Started).expect("a remove");
    assert_eq!(held(&cx, &dir, "f"), None);
    assert_eq!(io.disk().expect("a simulated disk").device_waits, waits);
}

/// A name sync is a real effect: `sync_names`, a synced remove and a synced close of a file
/// its handle made each wait for their directory's sync, and none is refused.
#[test]
fn a_directorys_names_are_synced_by_an_effect() {
    let io = world(25, Plant::None);
    let (cx, dir) = data(&io);
    let before = io.disk().expect("a simulated disk");
    let file = create(&cx, &dir, "made");
    let appended = file.append(&cx, b"bytes", Durable::No);
    let closed = file.close_synced(&cx);
    appended.expect("an append");
    closed.expect("a synced close of a file its handle made");
    dir.remove_synced(&cx, &path("made")).expect("a synced remove");
    dir.sync_names(&cx).expect("a directory's names synced");
    let after = io.disk().expect("a simulated disk");
    assert_eq!(after.name_syncs - before.name_syncs, 3);
    assert!(io.crash());
    assert_eq!(held(&cx, &dir, "made"), None, "a synced remove is kept through a crash");
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
    // The path is written again behind the `Flushed`, unsynced: the `Flushed` is stale, and
    // its clone is refused before anything is made, naming the file.
    dir.publish_unsynced(&cx, &path("base"), Content::bytes(&[8; 60]), Unsynced::No).expect("a publish");
    assert!(flushed.is_stale());
    let refused = flushed.clone_to(&cx, &dir, &path("stale")).expect_err("a clone of a file changed behind its Flushed");
    assert!(matches!(refused.kind, FsKind::Stale { .. }), "{refused}");
    assert_eq!(refused.op, FsOp::CloneTo);
    assert!(refused.to_string().contains("data/base") && refused.to_string().contains("flush it again"), "{refused}");
    assert_eq!(held(&cx, &dir, "stale"), None, "a refused clone made nothing");
    let counts = io.disk().expect("a simulated disk");
    assert_eq!((counts.clones, counts.forced_writeback), (1, 0));
    // Flushed again, it clones, and forces nothing.
    let again = crate::fs::Written::from(flushed).flush(&cx).expect("a flush");
    assert!(!again.is_stale());
    again.clone_to(&cx, &dir, &path("fresh")).expect("a clone of the file flushed again");
    let counts = io.disk().expect("a simulated disk");
    assert_eq!((counts.clones, counts.forced_writeback), (2, 0));
    assert_eq!(held(&cx, &dir, "fresh").as_deref(), Some(&[8u8; 60][..]));
    // A copy of the same unflushed file forces nothing: it reads and writes.
    dir.publish_unsynced(&cx, &path("base"), Content::bytes(&[9; 40]), Unsynced::No).expect("a publish");
    let chunk = NonZero::new(16).expect("a chunk");
    let unflushed = dir.open_read(&cx, &path("base")).expect("the file");
    let copied = dir.publish_unsynced(&cx, &path("copy"), Content::copy_of(&unflushed, chunk), Unsynced::No).expect("a copy");
    assert_eq!(copied.how, How::Copied);
    assert_eq!(io.disk().expect("a simulated disk").forced_writeback, 0);
    assert_eq!(held(&cx, &dir, "copy").as_deref(), Some(&[9u8; 40][..]));
}

/// The invariant: no clone made through a `Flushed` forces writeback, whatever was written
/// behind it. The oracle is the simulated disk's ledger. `plant` is the bug planted.
fn stale_clone_forces_nothing(plant: Plant) -> Result<(), String> {
    let io = world(24, plant);
    let (cx, dir) = data(&io);
    let flushed = dir.publish_synced(&cx, &path("base"), Content::bytes(&[7; 100])).map_err(|error| error.to_string())?.file;
    // Written behind the `Flushed`, three ways: a publish over the path, an append through
    // another handle, a rename onto it.
    let file = match dir.open(&cx, &path("base")) {
        Ok(file) => file,
        Err(error) => return Err(error.to_string()),
    };
    let appended = file.append_unsynced(&cx, &[8; 60], Unsynced::No);
    let closed = file.close_unsynced(Unsynced::No);
    appended.map_err(|error| error.to_string())?;
    closed.map_err(|error| error.to_string())?;
    // A clone either is refused or forced nothing out.
    let cloned = flushed.clone_to(&cx, &dir, &path("clone"));
    let published = dir.publish_unsynced(&cx, &path("published"), Content::file(&flushed), Unsynced::No);
    let forced = io.disk().ok_or("no simulated disk")?.forced_writeback;
    match forced {
        0 => Ok(()),
        forced => Err(format!(
            "clones of a file written behind its Flushed forced {forced} bytes out (clone_to: {}, publish: {})",
            cloned.is_ok(),
            published.is_ok()
        )),
    }
}

#[test]
fn a_flushed_file_written_behind_is_not_cloned() {
    stale_clone_forces_nothing(Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_a_stale_flushed_cloned() {
    let why = stale_clone_forces_nothing(Plant::StaleFlushedCloned).expect_err("a stale Flushed was cloned and nothing noticed");
    assert!(why.contains("forced 60 bytes") || why.contains("forced 120 bytes"), "{why}");
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
    // A close told `Started` begins the write-back and gives a `Written`.
    let file = create(&cx, &dir, "started");
    let closed = file.close(&cx, Durable::Started).expect("a close");
    assert!(matches!(closed, Placed::Written(_)), "{closed:?}");
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
    let mut sim = SimWorld::new(13, Limits::USUAL);
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

/// The bounds a world's waits, closes and trace end by are the ones its maker said: there is
/// no constant bstd picked. A trace told to keep two records keeps two and counts the rest:
/// the root's opening is a record too, so of five it counts three.
#[test]
fn a_worlds_limits_are_the_ones_its_maker_said() {
    let limits = Limits {
        abandon_wait: Duration::from_millis(250),
        poll_fuel: 1 << 10,
        close_most: 4,
        trace_most: 2,
    };
    let io = Io::simulated(SimWorld::new(21, limits));
    assert_eq!(io.limits(), limits);
    let cx = root(&io);
    for what in ["one", "two", "three", "four"] {
        cx.event(what);
    }
    let (kept, dropped) = io.trace(|trace| (trace.records().len(), trace.dropped()));
    assert_eq!((kept, dropped), (2, 3), "two kept, three counted");
    // The usual world's are the named value, said at its call.
    assert_eq!(world(21, Plant::None).limits(), Limits::USUAL);
}

/// A stream holds at most the bytes its opener said: a reply that would pass the bound is
/// refused whole, and received bytes make room again.
#[test]
fn a_streams_inbox_is_bounded_by_what_its_opener_said() {
    use crate::{hosted::Refused, streams::Streams, url::Url};
    let io = world(22, Plant::None);
    let cx = root(&io);
    let origin = Url::parse("https://relay.example/session").expect("a URL");
    let origin = origin.origin().expect("an origin");
    let echo = Streams::handler(|_: &crate::Cx, _: &crate::url::Origin, sent: &[u8], reply: &mut crate::mem::Bytes<'_>| {
        reply.try_extend(sent)
    });
    let mut heard = cx.bytes();
    // A stream is linear: finished whatever it carried, and checked after.
    let outcome = match echo.open(&cx, origin, 8) {
        Ok(mut stream) => {
            let fits = stream.send_all(&cx, b"12345");
            let over = stream.send_all(&cx, b"6789").map_err(|error| error.kind);
            let took = stream.recv(&cx, 64, &mut heard);
            let again = stream.send_all(&cx, b"6789");
            let finished = stream.finish(&cx);
            (fits.is_ok(), over, took, again.is_ok(), finished.is_ok())
        }
        Err(error) => panic!("a stream to a handler: {error}"),
    };
    assert_eq!(outcome, (true, Err(Refused::InboxFull { held: 5, most: 8 }), Ok(5), true, true));
    assert_eq!(heard.as_slice(), b"12345", "the refused reply left nothing behind");
}

/// A lock wait is a wait: it takes the task's context, and a lock still held at the
/// context's deadline ends the wait with the deadline, in a span named for the lock.
#[test]
fn a_lock_wait_takes_a_context_and_ends_at_its_deadline() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::sync::Lock;
    let io = world(23, Plant::None);
    let cx = root(&io);
    let clock = io.need_clock().expect("a clock");
    let lock = Lock::new("a test's lock", 7u32);
    match lock.lock(&cx) {
        Ok(guard) => assert_eq!(*guard, 7),
        Err(stopped) => panic!("a free lock: {stopped}"),
    }
    let (held, release) = (AtomicBool::new(false), AtomicBool::new(false));
    std::thread::scope(|threads| {
        threads.spawn(|| {
            let guard = lock.try_lock().expect("the lock is free");
            held.store(true, Ordering::SeqCst);
            // Bounded: the test's thread releases it after its one wait; 60 s at most.
            let start = std::time::Instant::now();
            while !release.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(60) {
                std::thread::yield_now();
            }
            drop(guard);
        });
        let start = std::time::Instant::now();
        while !held.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(60) {
            std::thread::yield_now();
        }
        assert!(lock.try_lock().is_none(), "held by the other thread");
        let short = cx.until(clock.after(Duration::from_millis(20)));
        let waited = lock.lock(&short).map(|_| ());
        release.store(true, Ordering::SeqCst);
        match waited {
            Err(Stopped::Deadline { span, .. }) => assert_eq!(span, "a test's lock"),
            other => panic!("a lock held past the deadline gave {other:?}"),
        }
    });
    // Free again: the same context takes it.
    assert!(lock.lock(&cx).is_ok());
    let said = io.trace(|trace| {
        trace.records().iter().any(|record| record.span == "a test's lock" && matches!(record.kind, RecordKind::Opened { .. }))
    });
    assert!(said, "the lock's wait opened a span named for it");
}
