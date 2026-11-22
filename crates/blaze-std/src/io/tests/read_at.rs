//! `File::read_at` through `Io::call` (wiring read-at-inline-1): on the native driver with
//! inline calls a small positional read is a `pread` on the calling thread, with no handoff to
//! the driver's threads; the simulation still sees and queues each read as an `Effect::Read`,
//! which it can fail, delay and reorder. A wrong fix (`read_at` left on `read_into(..).wait`,
//! which always queues) hands every native read over, and the first test fails on its count.
//!
//! - `a_native_read_at_hands_nothing_over` (macOS, the driver with inline calls): prints
//!   `read_at: 0 handoffs, 100 inline preads in 100 reads; the bytes equal`.
//! - `a_simulated_read_at_is_still_an_effect`: prints
//!   `read_at in the simulation: 100 Effect::Read queued in 100 reads; the bytes equal`.

use core::sync::atomic::Ordering;
use std::{println, vec::Vec};

use blaze_core::Effect;

use super::{path, root};
use crate::{
    fs::{Content, Unsynced},
    io::Io,
};

/// The reads each test makes.
const READS: u64 = 100;

/// What a run of reads cost the world: handoffs to the driver, calls made inline, and the
/// `Effect::Read`s recorded as asked.
struct Cost {
    handoffs: u64,
    inline: u64,
    asked_reads: usize,
}

/// A 4 KiB file published on `io`, then [`READS`] reads at scattered offsets, some running
/// past its end: each read's bytes are the file's, and what the reads alone cost.
fn reads(io: &Io) -> Cost {
    let cx = root(io);
    let fs = io.need_fs().expect("a world with files");
    let data: Vec<u8> = (0..4096u32).map(|it| (it % 251) as u8).collect();
    fs.root().publish_synced(&cx, &path("pack"), Content::bytes(&data)).expect("the pack is published");
    let file = fs.root().open(&cx, &path("pack")).expect("the pack is opened");
    let seen = io.seen().expect("a shared world");
    io.record();
    io.asked();
    let (handoffs, inline) = (seen.handoffs.load(Ordering::Relaxed), seen.inline.load(Ordering::Relaxed));
    for n in 0..READS {
        let at = n * 37 % 4000;
        let len = 1 + (n as usize * 13 % 120);
        let mut into = cx.bytes();
        let got = file.read_at(&cx, at, len, &mut into).expect("a read of the pack");
        let end = (at as usize + len).min(data.len());
        assert_eq!(got, end - at as usize, "read {n}: {len} bytes at {at}, fewer only at the end");
        assert_eq!(into.as_slice(), &data[at as usize..end], "read {n}: the pack's bytes at {at}");
    }
    let asked_reads = io.asked().iter().filter(|effect| matches!(effect, Effect::Read { .. })).count();
    let cost = Cost {
        handoffs: seen.handoffs.load(Ordering::Relaxed) - handoffs,
        inline: seen.inline.load(Ordering::Relaxed) - inline,
        asked_reads,
    };
    file.close_unsynced(Unsynced::No).expect("the pack is closed");
    cost
}

#[cfg(target_os = "macos")]
#[test]
fn a_native_read_at_hands_nothing_over() {
    let dir = blaze_core::temp::TempDir::for_test("bstd-read-at-");
    let io = super::native(&dir);
    let cost = reads(&io);
    assert_eq!(cost.handoffs, 0, "{} of {READS} native reads were handed to the driver's threads", cost.handoffs);
    assert_eq!(cost.inline, READS, "each native read is one inline call");
    println!("read_at: {} handoffs, {} inline preads in {READS} reads; the bytes equal", cost.handoffs, cost.inline);
}

#[test]
fn a_simulated_read_at_is_still_an_effect() {
    let io = Io::sim(9);
    let cost = reads(&io);
    assert_eq!(cost.inline, 0, "the simulation makes nothing inline");
    assert_eq!(cost.handoffs, READS, "each simulated read is handed to the simulation's queue");
    assert_eq!(cost.asked_reads, READS as usize, "each simulated read is asked as an Effect::Read");
    println!("read_at in the simulation: {} Effect::Read queued in {READS} reads; the bytes equal", cost.asked_reads);
}
