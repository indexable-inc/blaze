//! The VFS made durable, sans-IO: a [`Journal`] owns a [`Vfs`], turns each commit into records
//! appended to one file, and says which commits are durable once the sync after them completes.
//!
//! - **Records** are framed `MAGIC, length, kind, payload, checksum`. A node record carries what
//!   the node is made of, never its hash: loading interns it again, so a record is checked by
//!   construction and a node is written once however many roots share it. A head record names a
//!   workspace, the root it moved to and the journal's sequence number.
//! - **Group commit**: commits made while an append or a sync is in flight go out together in the
//!   next append, and one sync makes them all durable.
//! - **Failures**: a failed append or sync puts its batch back to be appended again. A record
//!   written twice loads once (a node is interned, a head keeps the highest sequence), and a batch
//!   half written by a failed append is skipped on load.
//! - **Recovery** reads the file from the start, skips a damaged stretch by looking for the next
//!   record whose checksum holds, and drops a head whose root's nodes did not survive. Before
//!   appending it cuts the file after the last good record and syncs the cut: a torn record
//!   followed by new ones can borrow their first bytes and pass its checksum, hiding them.

use std::sync::Arc;

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use rustc_hash::FxHashMap;

use crate::{Commit, CommitError, Hash, NodeId, Op, Vfs};

const MAGIC: [u8; 4] = *b"BZJ1";
const HEADER: usize = MAGIC.len() + 4 + 1;
const CHECKSUM: usize = 8;

const FILE: u8 = 1;
const DIR: u8 = 2;
const HEAD: u8 = 3;

/// A commit's place in line for durability.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

/// What loading a journal found.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Recovered {
    pub records: usize,
    /// Bytes that were not a whole record with a good checksum: a torn tail, or a batch a failed
    /// append left half written.
    pub skipped: usize,
    /// Head records whose root did not survive, dropped.
    pub heads_dropped: usize,
    /// Where the last record that loaded ends: the file is cut here before the next append.
    pub valid: usize,
}

struct Batch {
    bytes: Vec<u8>,
    tickets: Vec<Ticket>,
}

pub struct Journal {
    vfs: Vfs,
    file: FileName,
    /// By node id: whether a record of it has been made.
    written: Vec<bool>,
    /// The sequence of each workspace's head, as last written.
    sequence: u64,
    heads: FxHashMap<Box<str>, u64>,
    /// Records made and not yet handed to an append.
    open: Batch,
    appending: Option<Batch>,
    syncing: Option<Batch>,
    /// The length to cut the file to before appending, and whether that cut is in flight.
    cut: Option<u64>,
    cutting: bool,
    next_ticket: u64,
    tag_base: u64,
}

impl Journal {
    /// A journal in `file` holding `vfs`. `tags` is the first of the two tags it uses for its
    /// effects (`tags` to `tags + 2`), so it can share a driver with other state machines.
    pub fn new(vfs: Vfs, file: FileName, tags: u64) -> Journal {
        Journal {
            written: vec![false; vfs.tree.len()],
            vfs,
            file,
            sequence: 0,
            heads: FxHashMap::default(),
            open: Batch { bytes: Vec::new(), tickets: Vec::new() },
            appending: None,
            syncing: None,
            cut: None,
            cutting: false,
            next_ticket: 0,
            tag_base: tags,
        }
    }

    /// The effect that reads the journal for [`Journal::recover`].
    pub fn read(file: FileName, tag: Tag) -> Effect {
        Effect::ReadAll { file, tag }
    }

    /// The journal whose file holds `bytes`.
    pub fn recover(file: FileName, tags: u64, bytes: &[u8]) -> (Journal, Recovered) {
        let mut vfs = Vfs::new();
        let mut found = Recovered::default();
        let mut heads: FxHashMap<Box<str>, Vec<(u64, Hash)>> = FxHashMap::default();
        let mut at = 0;
        while at < bytes.len() {
            match record(&bytes[at..]) {
                Some((kind, payload, len)) => {
                    if load(&mut vfs, kind, payload, &mut heads).is_some() {
                        found.records += 1;
                        at += len;
                        found.valid = at;
                        continue;
                    }
                    found.skipped += len;
                    at += len;
                }
                None => {
                    let next = bytes[at + 1..]
                        .windows(MAGIC.len())
                        .position(|it| it == MAGIC)
                        .map_or(bytes.len(), |it| at + 1 + it);
                    found.skipped += next - at;
                    at = next;
                }
            }
        }
        let mut journal = Journal::new(vfs, file, tags);
        journal.written = vec![true; journal.vfs.tree.len()];
        journal.cut = (found.valid < bytes.len()).then_some(found.valid as u64);
        for (name, mut moves) in heads {
            // The latest move whose root survived; a later one whose nodes did not is dropped.
            moves.sort_unstable();
            while let Some((sequence, root)) = moves.pop() {
                journal.sequence = journal.sequence.max(sequence);
                let Some(root) = journal.vfs.tree.node(&root) else {
                    found.heads_dropped += 1;
                    continue;
                };
                journal.vfs.set_head(&name, root);
                journal.heads.insert(name, sequence);
                break;
            }
        }
        (journal, found)
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// The sequence number `workspace`'s head was last written with.
    pub fn sequence(&self, workspace: &str) -> Option<u64> {
        self.heads.get(workspace).copied()
    }

    /// Commits `ops` to `workspace` in memory, and records it to be made durable: the ticket is
    /// in what [`Journal::complete`] returns once it is.
    pub fn commit(
        &mut self,
        workspace: &str,
        base: Hash,
        ops: &[Op],
    ) -> Result<(Commit, Ticket), CommitError> {
        let commit = self.vfs.commit(workspace, base, ops)?;
        Ok((commit, self.record_head(workspace, commit.root)))
    }

    /// Moves `workspace` to `root` and records it, as a checkout does.
    pub fn set_head(&mut self, workspace: &str, root: NodeId) -> Ticket {
        self.vfs.set_head(workspace, root);
        self.record_head(workspace, root)
    }

    fn record_head(&mut self, workspace: &str, root: NodeId) -> Ticket {
        self.written.resize(self.vfs.tree.len(), false);
        self.write_node(root);
        self.sequence += 1;
        self.heads.insert(workspace.into(), self.sequence);
        let mut payload = Vec::with_capacity(8 + 32 + workspace.len());
        payload.extend_from_slice(&self.sequence.to_le_bytes());
        payload.extend_from_slice(&self.vfs.tree.hash(root).0);
        payload.extend_from_slice(workspace.as_bytes());
        frame(&mut self.open.bytes, HEAD, &payload);
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.open.tickets.push(ticket);
        ticket
    }

    /// Records `node` and everything under it not recorded yet, children first, so a record
    /// only names nodes recorded before it.
    fn write_node(&mut self, node: NodeId) {
        let mut stack = vec![(node, false)];
        while let Some((node, children_done)) = stack.pop() {
            if self.written[node.0 as usize] {
                continue;
            }
            let tree = &self.vfs.tree;
            if let Some(bytes) = tree.bytes(node) {
                frame(&mut self.open.bytes, FILE, bytes);
            } else if !children_done {
                stack.push((node, true));
                let entries = tree.entries(node).expect("a directory");
                stack.extend(
                    entries
                        .iter()
                        .filter(|it| !self.written[it.1.0 as usize])
                        .map(|it| (it.1, false)),
                );
                continue;
            } else {
                let entries = tree.entries(node).expect("a directory");
                let mut payload = Vec::new();
                for &(name, child) in entries {
                    let name = tree.name_str(name);
                    payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
                    payload.extend_from_slice(name.as_bytes());
                    payload.extend_from_slice(&tree.hash(child).0);
                }
                frame(&mut self.open.bytes, DIR, &payload);
            }
            self.written[node.0 as usize] = true;
        }
    }

    /// Starts the next append if none is in flight and there is something to append, after the
    /// cut recovery asked for.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        if self.appending.is_some() || self.syncing.is_some() || self.cutting {
            return;
        }
        if let Some(len) = self.cut {
            effects.push(Effect::Truncate {
                file: self.file.clone(),
                len,
                tag: Tag(self.tag_base + 2),
            });
            self.cutting = true;
            return;
        }
        if self.open.tickets.is_empty() && self.open.bytes.is_empty() {
            return;
        }
        let batch =
            std::mem::replace(&mut self.open, Batch { bytes: Vec::new(), tickets: Vec::new() });
        effects.push(Effect::Append {
            file: self.file.clone(),
            bytes: batch.bytes.clone(),
            tag: Tag(self.tag_base),
        });
        self.appending = Some(batch);
    }

    /// Whether `tag` is one of this journal's.
    pub fn owns(&self, tag: Tag) -> bool {
        (self.tag_base..self.tag_base + 3).contains(&tag.0)
    }

    /// Takes the completion of one of its effects; returns the tickets now durable.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let mut durable = Vec::new();
        match (completion.tag.0 - self.tag_base, completion.done) {
            (0, Done::Appended) => {
                self.syncing = self.appending.take();
                effects.push(Effect::Sync { file: self.file.clone(), tag: Tag(self.tag_base + 1) });
            }
            (1, Done::Synced) => {
                durable = self.syncing.take().map(|it| it.tickets).unwrap_or_default();
            }
            (2, Done::Truncated) => {
                self.cutting = false;
                self.cut = None;
                self.syncing = Some(Batch { bytes: Vec::new(), tickets: Vec::new() });
                effects.push(Effect::Sync { file: self.file.clone(), tag: Tag(self.tag_base + 1) });
            }
            (2, Done::Failed(_)) => self.cutting = false,
            (_, Done::Failed(_)) => {
                // After a failed sync the kernel may have dropped the pages it could not write and
                // a second sync can then succeed without them, so the batch is written again.
                let batch = self
                    .appending
                    .take()
                    .or_else(|| self.syncing.take())
                    .expect("a batch in flight");
                let later = std::mem::replace(&mut self.open, batch);
                self.open.bytes.extend_from_slice(&later.bytes);
                self.open.tickets.extend(later.tickets);
            }
            (_, done) => unreachable!("the journal asked for no {done:?}"),
        }
        self.flush(effects);
        durable
    }

    /// Drops every node no head and no root in `keep` reaches.
    pub fn collect(&mut self, keep: &mut Vec<NodeId>) {
        self.written.resize(self.vfs.tree.len(), false);
        let renumbered = self.vfs.collect(keep);
        let mut written = vec![false; self.vfs.tree.len()];
        for (old, new) in renumbered.into_iter().enumerate() {
            if let Some(new) = new {
                written[new.0 as usize] = self.written[old];
            }
        }
        self.written = written;
    }
}

fn frame(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(payload);
    out.extend_from_slice(&hasher.finalize().as_bytes()[..CHECKSUM]);
}

/// The record at the start of `bytes`, its payload and its whole length, if it is a whole record
/// with a good checksum.
fn record(bytes: &[u8]) -> Option<(u8, &[u8], usize)> {
    if bytes.len() < HEADER + CHECKSUM || bytes[..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    let kind = bytes[8];
    let end = HEADER.checked_add(len)?;
    let payload = bytes.get(HEADER..end)?;
    let checksum = bytes.get(end..end + CHECKSUM)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(payload);
    (hasher.finalize().as_bytes()[..CHECKSUM] == *checksum).then_some((
        kind,
        payload,
        end + CHECKSUM,
    ))
}

/// Loads one record; `None` when it names a node that is not there or is malformed.
fn load(
    vfs: &mut Vfs,
    kind: u8,
    payload: &[u8],
    heads: &mut FxHashMap<Box<str>, Vec<(u64, Hash)>>,
) -> Option<()> {
    match kind {
        FILE => {
            vfs.tree.file(Arc::from(payload));
        }
        DIR => {
            let mut entries = Vec::new();
            let mut rest = payload;
            while !rest.is_empty() {
                let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
                let name = std::str::from_utf8(rest.get(4..4 + len)?).ok()?;
                let hash = Hash(rest.get(4 + len..4 + len + 32)?.try_into().ok()?);
                rest = &rest[4 + len + 32..];
                let child = vfs.tree.node(&hash)?;
                let name = vfs.tree.name(name);
                entries.push((name, child));
            }
            vfs.tree.dir(entries);
        }
        HEAD => {
            let sequence = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
            let root = Hash(payload.get(8..40)?.try_into().ok()?);
            let name = std::str::from_utf8(&payload[40..]).ok()?;
            heads.entry(name.into()).or_default().push((sequence, root));
        }
        _ => return None,
    }
    Some(())
}
