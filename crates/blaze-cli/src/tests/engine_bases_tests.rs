//! The home's bases (engine_answer.rs `BASES_ROOT`, thoughts2 1.5), their sans-IO parts: the
//! workspace's identity from a lock, the record's form, and the list a run of saves leaves.
//!
//! THE INVARIANT: after any run of saves by checkouts of a few workspaces, a workspace with at
//! most [`BASES_PER_WORKSPACE`] layouts among at most [`BASES_MOST`] (workspace, layout) pairs
//! in all starts from the pointer its own last save kept (by its lock first), whatever the
//! other workspaces saved after it; the record holds at most the caps, newest first, each
//! (workspace, layout) once, and reads back equal. THE ORACLE is the run's history, a plain
//! list with no cap. PLANTED, CAUGHT: [`Keep::OnePointer`], the single home-wide pointer as
//! before 2026-11-21 (each save replaces the one entry), which a second workspace's save
//! breaks.

use std::format;
use std::vec::Vec as StdVec;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::*;

fn test_frame() -> Frame {
    Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a test frame")
}

fn id(byte: u8) -> ContentId {
    ContentId([byte; 32])
}

/// How a run of saves keeps its bases: the list (the default), or the control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keep {
    List,
    OnePointer,
}

/// One save of the history: a checkout of `workspace` (0..4) at `layout` (0..6) with `lock`
/// (0..3, 0 for none read) kept `pointer`.
#[derive(Clone, Copy, Debug)]
struct Save {
    workspace: u8,
    layout: u8,
    lock: u8,
    pointer: u8,
}

fn base_of(save: Save) -> Base {
    let lock = match save.lock {
        0 => NO_LOCK,
        n => id(0x40 + save.workspace * 8 + n),
    };
    // The root manifest moves with the layout: one root manifest a (workspace, layout).
    let root = id(0xC0 + save.workspace * 8 + save.layout);
    Base { workspace: id(0x10 + save.workspace), lock, layout: id(0x80 + save.workspace * 8 + save.layout), root, pointer: Key(id(save.pointer)), seq: 0 }
}

fn saves(tape: &Tape) -> StdVec<Save> {
    let mut out = StdVec::new();
    let count = tape.below(48, "saves in the run");
    for n in 0..count {
        out.push(Save {
            workspace: u8::try_from(tape.below(4, "a save's workspace")).expect("below 4"),
            layout: u8::try_from(tape.below(6, "a save's layout")).expect("below 6"),
            lock: u8::try_from(tape.below(3, "a save's lock")).expect("below 3"),
            pointer: u8::try_from(n + 1).expect("under 256 saves"),
        });
    }
    out
}

fn run(tape: &Tape, keep: Keep) -> Result<(), Broke> {
    let frame = test_frame();
    let history = saves(tape);
    let mut bases: StdVec<Base> = StdVec::new();
    for save in history.iter() {
        let next = match keep {
            Keep::List => with_base(&bases, base_of(*save), &frame),
            Keep::OnePointer => {
                let mut one = blaze_own::Vec::new_in(&frame);
                one.push(Base { seq: bases.first().map_or(0, |it| it.seq + 1), ..base_of(*save) });
                one
            }
        };
        let bytes = encode_bases(&next, &frame);
        let read = decode_bases(&bytes, &frame).ok_or_else(|| Broke::Oracle(format!("the record does not read back ({keep:?})")))?;
        if read.as_slice() != next.as_slice() {
            return Err(Broke::Oracle(format!("the record reads back other entries ({keep:?})")));
        }
        bases = next.iter().copied().collect();
    }
    // The caps, the order, each (workspace, layout) once.
    if bases.len() > BASES_MOST || !bases.windows(2).all(|pair| pair[0].seq > pair[1].seq) {
        return Err(Broke::Oracle(format!("past the caps or out of order: {} entries ({keep:?})", bases.len())));
    }
    for (at, base) in bases.iter().enumerate() {
        let of_workspace = bases.iter().filter(|it| it.workspace == base.workspace).count();
        let twice = bases[..at].iter().any(|it| it.workspace == base.workspace && it.layout == base.layout);
        if of_workspace > BASES_PER_WORKSPACE || twice {
            return Err(Broke::Oracle(format!("a workspace past its cap, or a layout twice ({keep:?})")));
        }
    }
    // The oracle: each workspace within the caps starts from its own last save's pointer.
    let mut pairs: StdVec<(u8, u8)> = history.iter().map(|it| (it.workspace, it.layout)).collect();
    pairs.sort_unstable();
    pairs.dedup();
    for workspace in 0..4u8 {
        let Some(last) = history.iter().rev().find(|it| it.workspace == workspace) else { continue };
        let layouts = pairs.iter().filter(|it| it.0 == workspace).count();
        if layouts > BASES_PER_WORKSPACE || pairs.len() > BASES_MOST {
            continue;
        }
        let want = base_of(*last);
        let got = pick_base(&bases, Workspace { id: want.workspace }, want.lock);
        if got.map(|it| it.pointer) != Some(want.pointer) {
            return Err(Broke::Oracle(format!("workspace {workspace} starts from {got:?}, not its last save's pointer {:?} ({keep:?})", want.pointer)));
        }
        // With no lock on disk, its root manifest finds the same answer (`pick_by_root`).
        if pick_by_root(&bases, want.root).map(|it| it.pointer) != Some(want.pointer) {
            return Err(Broke::Oracle(format!("workspace {workspace}'s root manifest starts it from another answer ({keep:?})")));
        }
        // With no read, its rows find it by its layout (the module's "KNOWN WITH NO READ").
        if workspace_of_rows(&bases, None, want.layout, want.lock) != Some(Workspace { id: want.workspace }) {
            return Err(Broke::Oracle(format!("workspace {workspace}'s layout does not name it ({keep:?})")));
        }
    }
    Ok(())
}

#[test]
fn each_workspace_starts_from_its_own_last_answer() {
    check(Config::cases(512), |tape| run(tape, Keep::List));
}

#[test]
fn control_one_pointer_for_the_home() {
    control(Config::cases(512), Keep::List, Keep::OnePointer, "starts from", run);
}

/// The pick: the newest of the workspace and lock, else the workspace's newest, never another
/// workspace's, and none for a workspace with no entry.
#[test]
fn the_pick_is_by_lock_then_workspace() {
    let frame = test_frame();
    let a = Workspace { id: id(1) };
    let entry = |workspace: u8, lock: ContentId, pointer: u8| Base { workspace: id(workspace), lock, layout: id(pointer), root: id(0x60 + pointer), pointer: Key(id(pointer)), seq: 0 };
    let mut bases = blaze_own::Vec::new_in(&frame);
    for base in [entry(1, id(7), 10), entry(1, id(8), 11), entry(2, id(7), 12)] {
        bases = with_base(&bases, base, &frame);
    }
    assert_eq!(pick_base(&bases, a, id(7)).map(|it| it.pointer), Some(Key(id(10))), "its lock's, though older");
    assert_eq!(pick_base(&bases, a, id(9)).map(|it| it.pointer), Some(Key(id(11))), "no entry of its lock: its newest");
    assert_eq!(pick_base(&bases, a, NO_LOCK).map(|it| it.pointer), Some(Key(id(11))), "no lock read: its newest, never a zero-lock match");
    assert_eq!(pick_base(&bases, Workspace { id: id(3) }, id(7)), None, "another workspace's lock is never picked");
    assert_eq!(pick_by_root(&bases, id(0x60 + 10)).map(|it| it.pointer), Some(Key(id(10))), "no lock on disk: the entry of its root manifest");
    assert_eq!(pick_by_root(&bases, id(0x60 + 99)), None, "a root manifest no entry has: none");
    assert_eq!(pick_by_root(&bases, NO_ROOT), None, "no root manifest: never a zero-root match");
}

/// The identity: path packages' names only, in any order, any version; a fetched package
/// changes nothing; two repositories of different members differ; a lock of no path package
/// has none; the members' own lock (graph.rs `members_lock`'s form) equals the real lock's.
#[test]
fn a_workspace_is_its_path_packages_names() {
    let frame = test_frame();
    let lock = |text: &str| workspace_of_lock(text.as_bytes(), &frame);
    let real = "version = 4\n\n[[package]]\nname = \"core\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n\n[[package]]\nname = \"cli\"\nversion = \"0.2.0\"\ndependencies = [\n \"core\",\n \"serde\",\n]\n";
    let members = "version = 4\n\n[[package]]\nname = \"cli\"\nversion = \"0.0.0\"\ndependencies = [\n \"core\",\n]\n\n[[package]]\nname = \"core\"\nversion = \"0.0.0\"\n";
    let other = "version = 4\n\n[[package]]\nname = \"reth\"\nversion = \"0.1.0\"\n";
    let fetched_only = "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    assert!(lock(real).is_some(), "a lock with path packages has a workspace");
    assert_eq!(lock(real), lock(members), "the members' own lock and the real one name one workspace");
    assert_ne!(lock(real), lock(other), "two repositories of different members differ");
    assert_eq!(lock(fetched_only), None, "a lock of no path package has none");
    assert_eq!(lock(""), None, "an empty lock has none");
    let bumped = real.replace("0.2.0", "0.3.0");
    assert_eq!(lock(real), lock(&bumped), "a version bump keeps the workspace");
}

/// A record of another form is refused, never half read: another magic, a count that is not
/// the bytes', or entries out of order.
#[test]
fn a_record_of_another_form_is_refused() {
    let frame = test_frame();
    let one = [
        Base { workspace: id(1), lock: id(2), layout: id(3), root: id(7), pointer: Key(id(4)), seq: 1 },
        Base { workspace: id(1), lock: id(2), layout: id(5), root: id(8), pointer: Key(id(6)), seq: 0 },
    ];
    let bytes = encode_bases(&one, &frame);
    assert_eq!(decode_bases(&bytes, &frame).map(|it| StdVec::from(it.as_slice())), Some(StdVec::from(one)));
    let mut other = StdVec::from(bytes.as_slice());
    other[3] = b'9';
    assert!(decode_bases(&other, &frame).is_none(), "another magic");
    assert!(decode_bases(&bytes[..bytes.len() - 1], &frame).is_none(), "a count that is not the bytes'");
    let swapped = [one[1], one[0]];
    assert!(decode_bases(&encode_bases(&swapped, &frame), &frame).is_none(), "entries out of order");
}
