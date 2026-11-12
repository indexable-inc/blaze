//! The effects of blaze-core that bstd has no call of its own for yet, each through bstd's
//! `Io` in a simulated world: what it completes with, which files it is recorded as naming,
//! and what the disk's ledger counts for it. `Io`, the ledger and `Pending` each match on
//! every `Effect` or `Done` with no catch-all, so an effect added to blaze-core fails bstd's
//! build until it has an arm in each; this file is where that arm's behaviour is then shown.
//!
//! - [`Effect::Barrier`]: orders what was written before what is written after, with no wait
//!   for the device's flush. The ledger counts a write-back and no flush, and the file is
//!   written out, so a clone after it forces nothing.
//! - [`Effect::Prewarm`]: asks the page cache to read ahead in a file what it holds of
//!   another. The simulation has no page cache: nothing is read, written or changed, both
//!   files are named, and a file that is not there fails.

use blaze_core::{Done, Effect, FileName};

use super::{root, world};
use crate::{Cx, Plant, io::Io};

fn name(raw: &str) -> FileName {
    FileName::new(raw).expect("a test's name is a name of blaze-core's")
}

/// `bytes` appended to `file`, not flushed.
fn append(io: &Io, cx: &Cx, file: &FileName, bytes: &[u8]) {
    // clone: the effect's name of the file, a refcount. alloc: the effect owns its bytes.
    let done = io.call(cx, |tag| Effect::Append { file: file.clone(), bytes: bytes.to_vec(), tag });
    assert!(matches!(done, Ok(Done::Appended)), "an append of {file}: {done:?}");
}

/// The bytes a clone of `file` forces out, in a world where `before` ran after an append of
/// three bytes to it.
fn forced_by_a_clone_after(seed: u64, before: impl FnOnce(&Io, &Cx, &FileName)) -> u64 {
    let io = world(seed, Plant::None);
    let cx = root(&io);
    let file = name("data/log");
    append(&io, &cx, &file, b"abc");
    before(&io, &cx, &file);
    let was = io.disk().expect("a simulated disk").forced_writeback;
    // clone: the effect's name of the file, a refcount.
    let done = io.call(&cx, |tag| Effect::Clone { from: file.clone(), to: name("data/copy"), tag });
    assert!(matches!(done, Ok(Done::Cloned)), "a clone of {file}: {done:?}");
    io.disk().expect("a simulated disk").forced_writeback - was
}

#[test]
fn a_barrier_orders_a_file_without_a_flush() {
    let io = world(31, Plant::None);
    let cx = root(&io);
    let file = name("data/log");
    append(&io, &cx, &file, b"abc");
    let before = io.disk().expect("a simulated disk");
    io.record();
    // clone: the effect's name of the file, a refcount.
    let done = io.call(&cx, |tag| Effect::Barrier { file: file.clone(), tag });
    assert!(matches!(done, Ok(Done::Ordered)), "a barrier: {done:?}");
    let after = io.disk().expect("a simulated disk");
    assert_eq!(
        (after.writebacks - before.writebacks, after.flushes, after.device_waits, after.written),
        (1, before.flushes, before.device_waits, before.written),
        "a barrier is a write-back: no flush, no wait for the device, no byte written"
    );
    assert_eq!(io.touched(), [(file, true)], "a barrier names its file");
    // The cause shown by removing it: the same clone with no barrier forces the three bytes.
    let barrier = |io: &Io, cx: &Cx, file: &FileName| {
        // clone: the effect's name of the file, a refcount.
        let done = io.call(cx, |tag| Effect::Barrier { file: file.clone(), tag });
        assert!(matches!(done, Ok(Done::Ordered)), "a barrier: {done:?}");
    };
    assert_eq!(forced_by_a_clone_after(32, barrier), 0, "the barrier wrote the file out");
    assert_eq!(forced_by_a_clone_after(32, |_, _, _| {}), 3, "with no barrier the clone does");
}

#[test]
fn a_prewarm_names_two_files_and_changes_nothing() {
    let io = world(33, Plant::None);
    let cx = root(&io);
    let (like, file) = (name("data/old"), name("data/new"));
    append(&io, &cx, &like, b"old image");
    append(&io, &cx, &file, b"new image");
    let before = io.disk().expect("a simulated disk");
    io.record();
    // clone: the effect's names of the files, refcounts.
    let done = io.call(&cx, |tag| Effect::Prewarm { like: like.clone(), file: file.clone(), tag });
    assert!(
        matches!(done, Ok(Done::Prewarmed { bytes: 0 })),
        "the simulation has no page cache, so nothing is asked for: {done:?}"
    );
    assert_eq!(io.disk().expect("a simulated disk"), before, "a hint: the ledger counts nothing");
    // clone: the expected names, refcounts.
    assert_eq!(io.touched(), [(like.clone(), false), (file.clone(), false)]);
    // A file like one that is not there holds nothing of it: 0 bytes, no failure.
    // clone: the effect's name of the file, a refcount.
    let fresh = io.call(&cx, |tag| Effect::Prewarm { like: name("data/never"), file: file.clone(), tag });
    assert!(matches!(fresh, Ok(Done::Prewarmed { bytes: 0 })), "the first file at a path: {fresh:?}");
    // The file to read ahead in has to be there.
    let missing = io.call(&cx, |tag| Effect::Prewarm { like, file: name("data/none"), tag });
    assert!(matches!(missing, Ok(Done::Failed(_))), "a prewarm of no file: {missing:?}");
}
