//! Any bytes as the journal file of `blaze_vfs::Vfs::recover`, with no model of what they hold;
//! the recovered VFS is then read, moved once more and settled through the same API, its effects
//! completed against the bytes as a disk would, and recovered again.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - recovery never panics, and asks for at most `blaze_fuzz::allowed(len)` bytes at once;
//! - its report adds up: each record it took is at least a header and a checksum long, and
//!   with the bytes it skipped they are no more than the file;
//! - each head it recovers is a root the tree has, with a sequence number, and every path under
//!   it lists, reads and follows without a panic;
//! - the same bytes recover to the same heads, sequences and report;
//! - the recovered VFS, moved once more and settled, makes that move durable and recovers to
//!   exactly its heads and sequences.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_core::{Completion, Done, Effect, FileName};
use blaze_fuzzer::Target;
use blaze_ids::ContentId;
use blaze_vfs::{Mode, Op, Recovered, Ticket, View, Vfs};

const TARGET: Target = Target {
    name: "vfs_journal_bytes",
    check,
    seeds,
    dictionary: &[
        b"BZJ4",
        &[1],
        &[2],
        &[3],
        &[4],
        &[5],
        &[0xff, 0xff, 0xff, 0xff],
        &[0xa4, 0x81, 0, 0],
        &[0xed, 0x81, 0, 0],
        &[0, 0xa0, 0, 0],
        &[0, 0x40, 0, 0],
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

/// The smallest record: magic, length, kind and checksum around an empty payload.
const RECORD: usize = 4 + 4 + 1 + 8;

/// The most paths one head's walk visits: a tree from the journal may name one directory many
/// times over, and so hold more paths than its records have bytes.
const WALK: usize = 1 << 10;

fn check(data: &[u8]) {
    let (vfs, found) = recover(data);
    assert!(
        found.records * RECORD + found.skipped <= data.len(),
        "the records and skipped bytes of a recovery are within the file: {found:?} of {} bytes",
        data.len()
    );
    let (again, found_again) = recover(data);
    assert_eq!(state(&again), state(&vfs), "the same bytes recover to the same heads");
    assert_eq!(found_again, found, "the same bytes recover with the same report");
    drop(again);

    let mut vfs = vfs;
    for (head, root, sequence) in state(&vfs) {
        assert!(vfs.has(root), "the recovered head {head:?} is at a root the tree has");
        assert!(sequence.is_some(), "the recovered head {head:?} has a sequence number");
        walk(&mut vfs, root);
    }
    moved_and_settled(vfs, data.to_vec(), data);
}

fn journal() -> FileName {
    FileName::new("vfs/journal").expect("a file name")
}

fn recover(bytes: &[u8]) -> (Vfs, Recovered) {
    let (largest, recovered) =
        blaze_fuzz::largest_allocation(|| Vfs::recover(journal(), 0, bytes));
    let allowed = blaze_fuzz::allowed(bytes.len());
    assert!(
        largest <= allowed,
        "recovering {} bytes asks for at most {allowed} at once, not {largest}",
        bytes.len()
    );
    recovered
}

/// Every head, its root and its sequence number, by name.
fn state(vfs: &Vfs) -> Vec<(String, ContentId, Option<u64>)> {
    vfs.heads()
        .into_iter()
        .map(|(head, root)| (head.to_owned(), root, vfs.sequence(head)))
        .collect()
}

/// Lists, reads and follows every path under `root`, up to [`WALK`] of them.
fn walk(vfs: &mut Vfs, root: ContentId) {
    let view = vfs.open(View { root }).expect("a head's root opens");
    let mut paths = vec![String::new()];
    let mut left = WALK;
    while let Some(path) = paths.pop() {
        if left == 0 {
            break;
        }
        left -= 1;
        _ = vfs.lookup(view, &path);
        _ = vfs.follow(view, &path);
        match vfs.list(view, &path) {
            Some(entries) => paths.extend(entries.into_iter().map(|(name, _)| {
                if path.is_empty() { name } else { format!("{path}/{name}") }
            })),
            None => _ = vfs.read(view, &path),
        }
    }
    vfs.close(view);
}

/// Moves a head once more (the first, or a new one), settles the journal on `file` and checks
/// it recovers to what `vfs` holds.
fn moved_and_settled(mut vfs: Vfs, mut file: Vec<u8>, data: &[u8]) {
    let head = vfs.heads().first().map_or_else(|| "fuzz".to_owned(), |it| it.0.to_owned());
    let base = vfs.head(&head).unwrap_or(vfs.empty());
    let write = Op::Write {
        mode: Mode::Regular,
        path: "fuzz".to_owned(),
        bytes: data[..data.len().min(16)].to_vec(),
    };
    let ticket = match vfs.commit(&head, base, &[write]) {
        Ok(commit) => commit.ticket,
        Err(_) => {
            let empty = vfs.empty();
            vfs.set_head(&head, empty).expect("the empty root is the tree's")
        }
    };
    let durable = settle(&mut vfs, &mut file);
    assert!(durable.contains(&ticket), "settling makes the move durable: {ticket:?} of {durable:?}");
    let (back, _) = recover(&file);
    assert_eq!(
        state(&back),
        state(&vfs),
        "a recovered journal, moved and settled, recovers to the heads it was left with"
    );
}

/// Completes every effect the journal asks for, as a disk holding `file` would, until it asks
/// for none; returns the tickets made durable.
fn settle(vfs: &mut Vfs, file: &mut Vec<u8>) -> Vec<Ticket> {
    let mut effects = Vec::new();
    let mut durable = Vec::new();
    vfs.flush(&mut effects);
    while let Some(effect) = effects.pop() {
        let tag = effect.tag();
        let done = match effect {
            Effect::Append { bytes, .. } => {
                file.extend_from_slice(&bytes);
                Done::Appended
            }
            Effect::Sync { .. } => Done::Synced,
            Effect::Truncate { len, .. } => {
                file.truncate(usize::try_from(len).expect("a length within the file"));
                Done::Truncated
            }
            Effect::Write { bytes, .. } => {
                *file = bytes;
                Done::Written
            }
            other => panic!("the journal asks only for appends, syncs, cuts and rewrites: {other:?}"),
        };
        durable.extend(vfs.complete(Completion { tag, done }, &mut effects));
    }
    durable
}

/// Journals the writer made: whole, torn in its last record, and rewritten by a collection.
fn seeds() -> Vec<Vec<u8>> {
    let write = |path: &str, bytes: &[u8], mode| Op::Write {
        mode,
        path: path.to_owned(),
        bytes: bytes.to_vec(),
    };
    let mut vfs = Vfs::recover(journal(), 0, &[]).0;
    let mut file = Vec::new();
    let mut out = Vec::new();
    let one = vfs
        .commit(
            "main",
            vfs.empty(),
            &[
                write("src/lib.rs", b"fn a() {}", Mode::Regular),
                write("bin/run", b"#!/bin/sh\n", Mode::Executable),
                write("link", b"src/lib.rs", Mode::Symlink),
            ],
        )
        .expect("a commit to a new head")
        .root;
    settle(&mut vfs, &mut file);
    out.push(file.clone());

    vfs.commit(
        "main",
        one,
        &[
            Op::Edit { at: 3, len: 1, path: "src/lib.rs".to_owned(), bytes: b"b".to_vec() },
            Op::Rename { to: "bin/go".to_owned(), from: "bin/run".to_owned() },
            Op::Delete { path: "link".to_owned() },
        ],
    )
    .expect("a commit on the head's root");
    vfs.set_head("pr/1", one).expect("a root the tree has");
    vfs.remove_head("pr/1").expect("a head just set");
    vfs.set_head("pr/2", one).expect("a root the tree has");
    settle(&mut vfs, &mut file);
    out.push(file.clone());
    out.push(file[..file.len() - 3].to_vec());

    vfs.collect(&[]);
    settle(&mut vfs, &mut file);
    out.push(file);
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
