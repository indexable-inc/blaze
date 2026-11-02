//! Journal records the input describes, framed as `crates/blaze-vfs/src/journal.rs` frames them
//! (magic, length, kind, payload, checksum) so each passes its checksum and reaches the loader,
//! recovered with `blaze_vfs::Vfs::recover`; the recovered VFS is then read, moved once more and
//! settled through the same API, and recovered again. The records are files, directories
//! naming earlier nodes by the content ID the tree gives them (any names, any modes, the same
//! name twice, one directory under many), head moves and removals with any sequence number,
//! records of any kind with any payload, unframed bytes between them, and a torn end.
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
use blaze_ids::{ContentId, Hasher};
use blaze_vfs::{DIR_DOMAIN, FILE_DOMAIN, Mode, Op, Recovered, Ticket, View, Vfs};

const TARGET: Target = Target {
    name: "vfs_journal_records",
    check,
    seeds,
    dictionary: &[
        &[2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[4, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
        b"BZJ4",
        b"..",
        b"/",
    ],
    max_len: 1 << 12,
    budget: Duration::from_secs(600),
};

/// The journal's framing, as `crates/blaze-vfs/src/journal.rs` writes it; this target only
/// reaches the loader while the two agree.
const MAGIC: [u8; 4] = *b"BZJ4";
const CHECKSUM_DOMAIN: &str = "blaze v2 vfs journal record 2";
const CHECKSUM: usize = 8;
const FILE: u8 = 1;
const DIR: u8 = 2;
const HEAD: u8 = 3;
const REMOVE: u8 = 4;
const SEQUENCE: u8 = 5;

/// git's mode of a directory entry.
const DIR_MODE: u32 = 0o040000;

/// Entry codes: the three file modes, a directory, the regular mode old gits wrote, a submodule
/// and nothing.
const CODES: [u32; 7] = [0o100644, 0o100755, 0o120000, DIR_MODE, 0o100664, 0o160000, 0];

/// The smallest record: magic, length, kind and checksum around an empty payload.
const RECORD: usize = 4 + 4 + 1 + CHECKSUM;

/// The most paths one head's walk visits: a directory named many times over holds more paths
/// than its records have bytes.
const WALK: usize = 1 << 10;

fn check(data: &[u8]) {
    let bytes = framed(data);
    let (vfs, found) = recover(&bytes);
    assert!(
        found.records * RECORD + found.skipped <= bytes.len(),
        "the records and skipped bytes of a recovery are within the file: {found:?} of {} bytes",
        bytes.len()
    );
    let (again, found_again) = recover(&bytes);
    assert_eq!(state(&again), state(&vfs), "the same bytes recover to the same heads");
    assert_eq!(found_again, found, "the same bytes recover with the same report");
    drop(again);

    let mut vfs = vfs;
    for (head, root, sequence) in state(&vfs) {
        assert!(vfs.has(root), "the recovered head {head:?} is at a root the tree has");
        assert!(sequence.is_some(), "the recovered head {head:?} has a sequence number");
        walk(&mut vfs, root);
    }
    moved_and_settled(vfs, bytes, data);
}

/// The input, read a byte at a time; past its end every byte is 0.
struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn done(&self) -> bool {
        self.at >= self.data.len()
    }

    fn byte(&mut self) -> u8 {
        let byte = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        byte
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    fn bytes(&mut self, len: usize) -> &'a [u8] {
        let start = self.at.min(self.data.len());
        let end = self.at.saturating_add(len).min(self.data.len());
        self.at = self.at.saturating_add(len);
        &self.data[start..end]
    }

    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[usize::from(self.byte()) % of.len()]
    }
}

/// Appends one record with a good checksum: a file's is over its content ID, any other's over
/// its payload.
fn frame(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&u32::try_from(payload.len()).expect("a short payload").to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    let id;
    let summed = if kind == FILE {
        id = ContentId::of(FILE_DOMAIN, payload);
        &id.0[..]
    } else {
        payload
    };
    let sum = Hasher::new(CHECKSUM_DOMAIN).update(&[kind]).update(summed).finish();
    out.extend_from_slice(&sum.0[..CHECKSUM]);
}

/// The journal the input describes, item by item, each led by a byte whose value mod 8 says
/// what it is:
///
/// - 0, a file: a length byte and that many bytes;
/// - 1, a directory: a count byte (mod 8), then per entry a name length byte (mod 8), the name,
///   a byte picking its code from [`CODES`] and a byte picking its child among the nodes so far;
/// - 2, a head move: a sequence number (8 bytes, little-endian), a byte picking its root among
///   the nodes so far, a name length byte (mod 8) and the name;
/// - 3, a removal: a sequence number, a name length byte (mod 8) and the name;
/// - 4, the sequence a rewrite was made at: a sequence number;
/// - 5, a record of any kind: a kind byte, a length byte and the payload;
/// - 6, unframed bytes: a length byte and the bytes;
/// - 7, a torn end: a byte (mod 32) of bytes cut off what is written so far.
///
/// The nodes are the empty root, then each file and directory in turn, with the content ID the
/// tree gives it (for a malformed directory, one it will not match).
fn framed(data: &[u8]) -> Vec<u8> {
    let mut input = Input { data, at: 0 };
    let mut nodes: Vec<(ContentId, bool)> = vec![(ContentId::of(DIR_DOMAIN, &[]), true)];
    let mut out = Vec::new();
    let mut payload = Vec::new();
    let mut hashed = Vec::new();
    while !input.done() {
        payload.clear();
        match input.byte() % 8 {
            0 => {
                let len = usize::from(input.byte());
                payload.extend_from_slice(input.bytes(len));
                frame(&mut out, FILE, &payload);
                nodes.push((ContentId::of(FILE_DOMAIN, &payload), false));
            }
            1 => {
                let mut entries: Vec<(&[u8], u32, ContentId)> = Vec::new();
                for _ in 0..input.byte() % 8 {
                    let len = input.byte() % 8;
                    let name = input.bytes(usize::from(len));
                    let code = input.pick(&CODES);
                    let (child, dir) = input.pick(&nodes);
                    payload.extend_from_slice(&u32::from(len).to_le_bytes());
                    payload.extend_from_slice(name);
                    payload.extend_from_slice(&code.to_le_bytes());
                    payload.extend_from_slice(&child.0);
                    let code = if dir { DIR_MODE } else { Mode::from_git(code).map_or(code, Mode::git) };
                    entries.push((name, code, child));
                }
                frame(&mut out, DIR, &payload);
                entries.sort_by(|a, b| a.0.cmp(b.0));
                hashed.clear();
                for (name, code, child) in &entries {
                    hashed.extend_from_slice(&code.to_le_bytes());
                    hashed.extend_from_slice(&(name.len() as u64).to_le_bytes());
                    hashed.extend_from_slice(name);
                    hashed.extend_from_slice(&child.0);
                }
                nodes.push((ContentId::of(DIR_DOMAIN, &hashed), true));
            }
            2 => {
                payload.extend_from_slice(&input.u64().to_le_bytes());
                payload.extend_from_slice(&input.pick(&nodes).0.0);
                let len = usize::from(input.byte() % 8);
                payload.extend_from_slice(input.bytes(len));
                frame(&mut out, HEAD, &payload);
            }
            3 => {
                payload.extend_from_slice(&input.u64().to_le_bytes());
                let len = usize::from(input.byte() % 8);
                payload.extend_from_slice(input.bytes(len));
                frame(&mut out, REMOVE, &payload);
            }
            4 => frame(&mut out, SEQUENCE, &input.u64().to_le_bytes()),
            5 => {
                let kind = input.byte();
                let len = usize::from(input.byte());
                frame(&mut out, kind, input.bytes(len));
            }
            6 => {
                let len = usize::from(input.byte());
                out.extend_from_slice(input.bytes(len));
            }
            _ => {
                let cut = usize::from(input.byte() % 32);
                out.truncate(out.len().saturating_sub(cut));
            }
        }
    }
    out
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

/// Descriptions of journals a writer could make and of ones only damage or a forger could.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    // A file, a directory holding it as `a`, and a head on that directory.
    let mut one = vec![0, 3, b'x', b'y', b'z', 1, 1, 1, b'a', 0, 1];
    one.extend([2, 1, 0, 0, 0, 0, 0, 0, 0, 2, 4]);
    one.extend(b"main");
    out.push(one.clone());
    // The same, moved to the empty root and removed, with a rewrite's sequence after.
    let mut moved = one.clone();
    moved.extend([2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 4]);
    moved.extend(b"main");
    moved.extend([3, 3, 0, 0, 0, 0, 0, 0, 0, 4]);
    moved.extend(b"main");
    moved.extend([4, 3, 0, 0, 0, 0, 0, 0, 0]);
    out.push(moved);
    // Directories each naming the one before twice, under a head.
    let mut shared = vec![0, 1, b'f'];
    for child in 1..12u8 {
        shared.extend([1, 2, 1, b'a', if child == 1 { 0 } else { 3 }, child]);
        shared.extend([1, b'b', if child == 1 { 0 } else { 3 }, child]);
    }
    shared.extend([2, 1, 0, 0, 0, 0, 0, 0, 0, 12, 1, b'w']);
    out.push(shared);
    // A directory naming one file twice, with two modes, and one named with a slash and `..`.
    let mut odd = vec![0, 1, b'f', 1, 2, 1, b'a', 0, 1, 1, b'a', 1, 1];
    odd.extend([1, 2, 3, b'a', b'/', b'b', 0, 1, 2, b'.', b'.', 2, 1]);
    odd.extend([2, 1, 0, 0, 0, 0, 0, 0, 0, 2, 1, b'x', 2, 2, 0, 0, 0, 0, 0, 0, 0, 3, 1, b'y']);
    out.push(odd);
    // The head record torn, then a stray magic and a record of no kind the journal has.
    let mut torn = one;
    torn.extend([7, 3, 6, 4]);
    torn.extend(MAGIC);
    torn.extend([5, 9, 2, 1, 2]);
    out.push(torn);
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
