//! The file-durability invariant (docs/v2/plan/design/std.md, "Files: whether bytes reached
//! the disk is in the type"), on the simulated disk and its ledger ([`crate::disk`]), through
//! publishes of bytes, clones, copies and patches, removes and crashes, with completions that
//! come late:
//!
//! 1. Once a publish or a remove has returned, a read of its path gives exactly what it
//!    said, whatever its durability.
//! 2. After a crash, a path whose last change was synced (`Durable::Yes`) holds exactly that
//!    change.
//! 3. After a crash, a path changed unsynced since holds what was last synced, or one of
//!    those unsynced changes cut short (a prefix of its bytes; nothing, for a remove): never
//!    anything else.
//! 4. A clone of a [`Flushed`] forces no writeback: no act of this test adds to
//!    `forced_writeback`, since the types leave no way to clone a file that is not flushed.
//! 5. No call told `Durable::No` waits for the device.
//! 6. A publish's result says what its durability made: a `Flushed` for `Yes`, a `Written`
//!    for `No`, and how the content got there.
//! 7. A call told `Durable::Started` makes its change, begins the write-back and waits for
//!    nothing: what a crash keeps of it is what it keeps of a `No`.
//!
//! Plants: [`Plant::PublishBeforeRename`] (a publish that reports done before its rename),
//! [`Plant::DurableSkipsNameSync`] (a `Durable::Yes` that skipped the directory sync) and
//! [`Plant::FlushedBeforeFlush`] (a `Flushed` handed out before the flush completed).

use std::{num::NonZero, time::Duration};

use blaze_core::sim::Faults;
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root};
use crate::{
    Cx, Plant,
    disk::DiskCounts,
    fs::{Content, Dir, Durable, Flushed, FsError, FsKind, How, Patch, Placed, ReadWrite},
    io::{Io, Limits, SimWorld},
};

const PATHS: [&str; 3] = ["a", "b", "deep/c"];

const DURABLE: [Durable; 3] = [Durable::No, Durable::Started, Durable::Yes];

#[derive(Clone, Copy, Debug)]
enum Act {
    /// Bytes published at a path.
    Publish { at: usize, len: usize, durable: Durable },
    /// A path published as a clone of another's flushed file, when the test holds one.
    Clone { at: usize, from: usize, durable: Durable },
    /// A path published as a copy of another's file, when there is one.
    Copy { at: usize, from: usize, chunk: usize, durable: Durable },
    /// A path published, synced, as another's flushed file with one range changed. Synced
    /// only: an unsynced patch cut short by a crash is its base with part of the range, which
    /// is no prefix of its content and is not this oracle's to describe.
    Derive { at: usize, from: usize, offset: u64, len: usize },
    Remove { at: usize, durable: Durable },
    Read { at: usize },
    Crash,
}

impl Act {
    /// The path the act changes or reads, and how durably it was told to.
    fn told(self) -> Option<(usize, Durable)> {
        match self {
            Act::Publish { at, durable, .. }
            | Act::Clone { at, durable, .. }
            | Act::Copy { at, durable, .. }
            | Act::Remove { at, durable } => Some((at, durable)),
            Act::Derive { at, .. } => Some((at, Durable::Yes)),
            Act::Read { at } => Some((at, Durable::No)),
            Act::Crash => None,
        }
    }
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    acts: Vec<Act>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let latency_us = d.pick(&[0, 50, 2_000]);
    let acts = (0..d.int(1, 40))
        .map(|_| {
            let at = d.int(0, 2) as usize;
            let durable = d.pick(&DURABLE);
            match d.int(0, 13) {
                0..=4 => Act::Publish { at, len: d.int(0, 48) as usize, durable },
                5 => Act::Clone { at, from: d.int(0, 2) as usize, durable },
                6 => Act::Copy { at, from: d.int(0, 2) as usize, chunk: d.int(1, 32) as usize, durable },
                7 => Act::Derive { at, from: d.int(0, 2) as usize, offset: d.int(0, 56), len: d.int(0, 16) as usize },
                8 => Act::Remove { at, durable },
                9 | 10 => Act::Read { at },
                _ => Act::Crash,
            }
        })
        .collect();
    Plan { seed, latency_us, acts }
}

/// What the model knows of one path.
#[derive(Default)]
struct Slot {
    /// What a read gives now; `None` when nothing is there.
    now: Option<Vec<u8>>,
    /// What the last synced change left: what a crash keeps if nothing unsynced followed.
    durable: Option<Vec<u8>>,
    /// Each unsynced change since, oldest first; `None` a remove.
    since: Vec<Option<Vec<u8>>>,
    /// The path's file as flushed, while nothing changed the path after it was handed out.
    flushed: Option<Flushed>,
}

impl Slot {
    /// The path was made to hold `content`, as durably as `durable` says.
    fn changed(&mut self, content: Option<Vec<u8>>, durable: Durable, flushed: Option<Flushed>) {
        match durable {
            Durable::Yes => {
                // clone: the model's copy of what a crash must keep.
                self.durable = content.clone();
                self.since.clear();
            }
            // clone: the model's copy of what a crash may keep a part of.
            Durable::No | Durable::Started => self.since.push(content.clone()),
        }
        self.now = content;
        self.flushed = flushed;
    }

    /// Whether a crash may leave the path holding `got` (invariants 2 and 3).
    fn allows(&self, got: &Option<Vec<u8>>) -> bool {
        if *got == self.durable {
            return true;
        }
        self.since.iter().any(|change| match (change, got) {
            (None, None) => true,
            (Some(content), Some(got)) => content.starts_with(got),
            _ => false,
        })
    }
}

/// What `at` holds, by a read through `dir`: `None` when nothing is there.
fn read(cx: &Cx, dir: &Dir<ReadWrite>, at: usize) -> Result<Option<Vec<u8>>, String> {
    let mut into = cx.bytes();
    match dir.read_all(cx, &path(PATHS[at]), &mut into) {
        Ok(_) => Ok(Some(into.as_slice().to_vec())),
        Err(error) if error.kind == FsKind::NotFound => Ok(None),
        Err(error) => Err(format!("harness: a read of {}: {error}", PATHS[at])),
    }
}

fn counts(io: &Io) -> Result<DiskCounts, String> {
    io.disk().ok_or_else(|| "harness: the world is not simulated".to_owned())
}

/// That a publish's result is typed as its durability says (invariant 6): the file as
/// flushed for `Durable::Yes`, nothing for the others.
fn typed(act: Act, placed: Placed, durable: Durable) -> Result<Option<Flushed>, String> {
    match (placed, durable) {
        (Placed::Flushed(flushed), Durable::Yes) => Ok(Some(flushed)),
        (Placed::Written(_), Durable::No | Durable::Started) => Ok(None),
        (placed, _) => Err(format!("{act:?} gave {placed:?}: not what its durability says")),
    }
}

/// What a call told `durable` made (invariant 7: `Started` makes its change as `No` and
/// `Yes` do).
fn as_told<T>(step: usize, act: Act, _durable: Durable, called: Result<T, FsError>) -> Result<Option<T>, String> {
    match called {
        Ok(made) => Ok(Some(made)),
        Err(error) => Err(format!("harness: act {step} ({act:?}): {error}")),
    }
}

fn took(act: Act, how: How, wanted: How) -> Result<(), String> {
    match how == wanted {
        true => Ok(()),
        false => Err(format!("{act:?} says it {how:?}, not {wanted:?}")),
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant).expect("a planted world");
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let mut slots: [Slot; 3] = Default::default();
    let mut written: u8 = 0;
    let mut next = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                written = written.wrapping_add(1);
                written
            })
            .collect()
    };
    for (step, &act) in plan.acts.iter().enumerate() {
        let before = counts(&io)?;
        let harness = |error: FsError| format!("harness: act {step} ({act:?}): {error}");
        match act {
            Act::Publish { at, len, durable } => {
                let bytes = next(len);
                let called = dir.publish(&cx, &path(PATHS[at]), Content::bytes(&bytes), durable);
                if let Some(published) = as_told(step, act, durable, called)? {
                    took(act, published.how, How::Written)?;
                    let flushed = typed(act, published.file, durable)?;
                    slots[at].changed(Some(bytes), durable, flushed);
                }
            }
            Act::Clone { at, from, durable } => {
                // clone: the test's handle on the flushed file, refcounts; and the model's copy.
                let (Some(base), Some(content)) = (slots[from].flushed.clone(), slots[from].now.clone()) else {
                    continue;
                };
                let called = dir.publish(&cx, &path(PATHS[at]), Content::file(&base), durable);
                if let Some(published) = as_told(step, act, durable, called)? {
                    took(act, published.how, How::Cloned)?;
                    let flushed = typed(act, published.file, durable)?;
                    slots[at].changed(Some(content), durable, flushed);
                }
            }
            Act::Copy { at, from, chunk, durable } => {
                // clone: the model's copy of what is copied.
                let Some(content) = slots[from].now.clone() else { continue };
                let chunk = NonZero::new(chunk).ok_or("harness: a chunk of no bytes")?;
                let of = dir.open_read(&cx, &path(PATHS[from])).map_err(harness)?;
                let called = dir.publish(&cx, &path(PATHS[at]), Content::copy_of(&of, chunk), durable);
                if let Some(published) = as_told(step, act, durable, called)? {
                    took(act, published.how, How::Copied)?;
                    let flushed = typed(act, published.file, durable)?;
                    slots[at].changed(Some(content), durable, flushed);
                }
            }
            Act::Derive { at, from, offset, len } => {
                // clone: the test's handle on the flushed file, refcounts; and the model's copy.
                let (Some(base), Some(mut content)) = (slots[from].flushed.clone(), slots[from].now.clone()) else {
                    continue;
                };
                let bytes = next(len);
                let (start, end) = (offset as usize, offset as usize + bytes.len());
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[start..end].copy_from_slice(&bytes);
                let patches = [Patch { at: crate::mem::Offset::new(offset), bytes: &bytes }];
                let published =
                    dir.publish_synced(&cx, &path(PATHS[at]), Content::patch_of(&base, &patches)).map_err(harness)?;
                took(act, published.how, How::Derived)?;
                slots[at].changed(Some(content), Durable::Yes, Some(published.file));
            }
            Act::Remove { at, durable } => {
                let called = dir.remove(&cx, &path(PATHS[at]), durable);
                if as_told(step, act, durable, called)?.is_some() {
                    slots[at].changed(None, durable, None);
                }
            }
            Act::Read { .. } => {}
            Act::Crash => {
                if !io.crash() {
                    return Err("harness: the world is not simulated".to_owned());
                }
                for (at, slot) in slots.iter_mut().enumerate() {
                    let got = read(&cx, &dir, at)?;
                    if !slot.allows(&got) {
                        return Err(format!(
                            "act {step}: after a crash {} holds {:?}: not the {:?} last synced, nor one of the {} \
                             unsynced changes since cut short",
                            PATHS[at],
                            got,
                            slot.durable,
                            slot.since.len()
                        ));
                    }
                    // What survived is on the disk: durable from here. A file read after a
                    // crash is flushed, but the test hands itself no `Flushed` the API did not.
                    // clone: the model's two copies of what survived.
                    *slot = Slot { now: got.clone(), durable: got, since: Vec::new(), flushed: None };
                }
            }
        }
        let after = counts(&io)?;
        // Invariant 4: nothing here can clone a file that is not flushed.
        if after.forced_writeback != before.forced_writeback {
            return Err(format!(
                "act {step} ({act:?}): a clone forced {} bytes out: its source was handed out as flushed and was not",
                after.forced_writeback - before.forced_writeback
            ));
        }
        let Some((at, durable)) = act.told() else { continue };
        // Invariant 5: only `Durable::Yes` waits for the device (`Started` begins and goes on).
        if durable != Durable::Yes && after.device_waits != before.device_waits {
            return Err(format!(
                "act {step} ({act:?}): told {durable:?}, it waited for the device {} times",
                after.device_waits - before.device_waits
            ));
        }
        // Invariant 1: what the call said is what a read gives, as soon as it has returned.
        let got = read(&cx, &dir, at)?;
        if got != slots[at].now {
            return Err(format!(
                "act {step} ({act:?}): {} reads as {:?} once the call returned, not the {:?} it said",
                PATHS[at],
                got.as_ref().map(Vec::len),
                slots[at].now.as_ref().map(Vec::len)
            ));
        }
    }
    Ok(())
}

/// The invariant: a file call's durability is what its caller said and nothing else. A read
/// after a publish or a remove returned gives what it said; a crash keeps exactly what was
/// synced, and of what was not, the last synced state or an unsynced change cut short; a
/// clone of a `Flushed` forces no writeback; nothing told `No` or `Started` waits for the
/// device; and `Started` keeps through a crash no more than `No` does.
#[hegel::test(test_cases = 200)]
fn files_keep_what_their_calls_said_through_crashes(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_publish_that_reports_done_before_its_rename() {
    control(Plant::PublishBeforeRename, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn the_oracle_catches_a_durable_yes_that_skipped_the_directory_sync() {
    control(Plant::DurableSkipsNameSync, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn the_oracle_catches_a_flushed_handed_out_before_the_flush_completed() {
    control(Plant::FlushedBeforeFlush, |seeds: &mut Seeds| plan(seeds), run);
}
