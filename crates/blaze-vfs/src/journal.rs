//! The VFS made durable, sans-IO: each head move becomes records appended to one file, and a
//! commit is durable once the sync after its records completes.
//!
//! - **Records** are framed `MAGIC, length, kind, payload, checksum`. A node record carries what
//!   the node is made of, never its ID: loading interns it again, so a record is checked by
//!   construction and a node is written once however many roots share it. A head record names a
//!   head, the root it moved to and the journal's sequence number; a removal names a head and
//!   a sequence number. A file record's checksum is over its content ID, not its bytes: the
//!   tree has the ID when it writes one and loading computes it to intern the file, so each
//!   byte is hashed once either way, and the ID commits to every byte as a checksum over them
//!   would.
//! - **Group commit**: commits made while an append or a sync is in flight go out together in the
//!   next append (its bytes moved into the effect, not copied), and one sync makes them all
//!   durable.
//! - **Failures**: a failed append or sync asks for a rewrite (below), which holds the heads as
//!   they are then and so every move the batch recorded; its tickets go with it. A batch half
//!   written by a failed append is replaced with the rest of the file, and a record written
//!   twice loads once (a node is interned, a head keeps the highest sequence).
//! - **Recovery** loads a prefix: it reads records from the start and stops at the first that
//!   is not whole, fails its checksum or is malformed. It never looks for a record past one it
//!   refused, as a torn record's payload may hold whole records of another journal (a file that
//!   is one) that would verify. Before appending it cuts the file after the last good record and
//!   syncs the cut, so nothing past it (a record naming a node it lost) loads later once that
//!   node is made again; a torn record followed by new ones could also borrow their first bytes
//!   and pass its checksum, hiding them. A head whose root's nodes are not there (only a forged
//!   file has one) is dropped, and the file is rewritten as the heads recovered.
//! - **Compaction**: after a collection, once as many bytes were appended since the file was
//!   last rewritten as that rewrite held, the next flush replaces the whole file, in one
//!   [`Effect::Write`], with the nodes the heads reach and one record per head: each rewrite
//!   is paid for by the appends before it, so a daemon that collects after every job writes
//!   each byte a constant number of times, and the file stays within about twice what the
//!   heads reach. Records of nodes a collection dropped stay in the file until then; each
//!   still makes its node, and a head keeps its latest record. Nothing is appended while a
//!   rewrite is in flight; a failed one is made again from the state then.
//! - **An image** (docs/v2/plan/design/restore.md): once the daemon's image holding the tree is
//!   durable ([`Journal::durable`]), the next flush rewrites the file to start with a base
//!   record naming it (the hash of its tree and its node count), then only what the image does
//!   not hold. A start maps the image and reads only that tail. The image records the base the
//!   file started from when it was written, so a crash after the image's rename and before the
//!   rewrite loads the image and the old file whole over it: every record of the old file is
//!   of a node the image has or one the file makes, because an image is written only while no
//!   node of the file's base was dropped since the file was last rewritten (`settled`).

use std::sync::Arc;

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_format::cursor::{Put as _, Reader};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher, bits::BitSet};

use crate::{
    FILE_DOMAIN, Mode, Recovered, Ticket,
    image::Bytes,
    tree::{DIR_MODE, NodeId, Tree, check},
};

const MAGIC: [u8; 4] = *b"BZJ4";
pub(crate) const HEADER: usize = MAGIC.len() + 4 + 1;
const CHECKSUM: usize = 8;
const CHECKSUM_DOMAIN: &str = "blaze v2 vfs journal record 2";

const FILE: u8 = 1;
const DIR: u8 = 2;
const HEAD: u8 = 3;
const REMOVE: u8 = 4;
/// The sequence a rewrite was made at, so the numbers of removals it drops are not given again.
const SEQUENCE: u8 = 5;
/// The image the file starts from: the hash of its tree and its node count. Only first.
const BASE: u8 = 6;
const BASE_PAYLOAD: usize = 32 + 8;

/// Sequence numbers are below this: one more per head move from 0 never reaches it, so a
/// record at or above it is forged, and the next number never overflows.
const SEQUENCES: u64 = 1 << 63;

const APPEND: u64 = 0;
const SYNC: u64 = 1;
const CUT: u64 = 2;
const REWRITE: u64 = 3;
const TAGS: u64 = 4;

#[derive(Default)]
struct Batch {
    bytes: Vec<u8>,
    tickets: Vec<Ticket>,
}

/// What is in flight: at most one effect at a time, so the file's order is the order asked.
/// Each holds the tickets it makes durable.
enum Flight {
    Idle,
    Appending(Vec<Ticket>),
    Syncing(Vec<Ticket>),
    Cutting,
    Rewriting(Vec<Ticket>),
}

/// By node, one bit: whether a record of it is in the file (or the image it starts from), or
/// will be before any record made now.
type Written = BitSet<NodeId>;

/// The same bits by each node's number after a collection, and whether a node with its bit
/// set was dropped.
fn renumbered(written: &Written, renumbered: &[Option<NodeId>], len: usize) -> (Written, bool) {
    let mut out = Written::new(len);
    let mut dropped = false;
    for (old, new) in renumbered.iter().enumerate() {
        if written.contains(NodeId(old as u32)) {
            match new {
                Some(new) => {
                    out.insert(*new);
                }
                None => dropped = true,
            }
        }
    }
    (out, dropped)
}

/// An image of the tree the file starts from, or will: its name, its node count, and which
/// nodes of the tree are in it (renumbered by collections).
struct FileBase {
    name: ContentId,
    nodes: u64,
    of: Written,
}

/// The tree read in place from an image, for [`Journal::restore`] to load the file over. It
/// has no heads: the file names every head (a base record is only written by a rewrite, which
/// records each head), so one removed since the image was written stays removed.
pub(crate) struct FromImage {
    pub(crate) tree: Tree,
    /// The hash of the image's tree and its node count, as a base record names them.
    pub(crate) name: ContentId,
    pub(crate) nodes: u64,
    /// The file does not start from this image but from the one it was written over (or
    /// none): the rewrite naming it did not happen, so the whole file is loaded over it and
    /// the next flush rewrites it.
    pub(crate) resumed: bool,
}

pub(crate) struct Journal {
    file: FileName,
    tags: u64,
    written: Written,
    sequence: u64,
    /// The sequence of each head, as last written.
    heads: HashMap<Box<str>, u64>,
    /// Records made and not yet handed to an append.
    open: Batch,
    flight: Flight,
    /// The length to cut the file to before appending.
    cut: Option<u64>,
    /// A rewrite is asked for at the next flush.
    compact: bool,
    /// Something was appended since the file was last rewritten.
    dirty: bool,
    /// Bytes appended since the file was last rewritten (a recovered file's, all of it).
    appended: u64,
    /// The length of the file's last rewrite, once it completed; 0 for none.
    rewritten: u64,
    /// The length of the rewrite in flight.
    rewriting: u64,
    next_ticket: u64,
    /// The image the file starts with: its nodes are never recorded.
    base: Option<FileBase>,
    /// The image [`Journal::stage`] was told of, not yet durable.
    staged: Option<FileBase>,
    /// A durable image the next rewrite starts the file with.
    next: Option<FileBase>,
    /// The image the rewrite in flight starts the file with.
    switching: Option<FileBase>,
    /// No node of the file's base was dropped since the file was last rewritten, so every
    /// node the file names is the tree's or the file's own.
    settled: bool,
    /// The rewrite in flight leaves the file settled.
    rewrite_settles: bool,
    /// The nodes [`write_node`] has still to record, reused.
    stack: Vec<(NodeId, bool)>,
}

/// What [`Journal::recover`] rebuilt.
pub(crate) struct Loaded {
    pub(crate) tree: Tree,
    pub(crate) heads: HashMap<Box<str>, NodeId>,
    pub(crate) journal: Journal,
    pub(crate) found: Recovered,
}

impl Journal {
    /// The journal whose file holds `bytes`, and the tree and heads it holds. A file that
    /// starts from an image loads only the records that name no node of it (none past the
    /// first that does), and is rewritten as what loaded.
    pub(crate) fn recover(file: FileName, tags: u64, bytes: &[u8]) -> Loaded {
        Journal::load(file, tags, bytes, None)
    }

    /// The journal whose file holds `bytes`, loaded over `image`: the file's records after its
    /// base record, each node of the image found where it is.
    pub(crate) fn restore(file: FileName, tags: u64, bytes: &[u8], image: FromImage) -> Loaded {
        Journal::load(file, tags, bytes, Some(image))
    }

    fn load(file: FileName, tags: u64, bytes: &[u8], image: Option<FromImage>) -> Loaded {
        let mut found = Recovered::default();
        let mut moves: HashMap<Box<str>, Vec<(u64, Option<ContentId>)>> = HashMap::default();
        let mut sequence = 0;
        let (mut tree, from) = match image {
            Some(FromImage { tree, name, nodes, resumed }) => {
                let of = Written::filled(tree.table().base_len() as usize);
                (tree, Some((FileBase { name, nodes, of }, resumed)))
            }
            None => (Tree::new(), None),
        };
        let mut valid = 0;
        let mut lost_base = false;
        if base_of(bytes).is_some() {
            valid = record(bytes).expect("a base record is a record").len;
            found.records += 1;
            lost_base = from.is_none();
        }
        while let Some(one) = record(&bytes[valid..]) {
            if load(&mut tree, &one, &mut moves, &mut sequence).is_none() {
                break;
            }
            found.records += 1;
            valid += one.len;
        }
        found.skipped = bytes.len() - valid;
        let mut journal = Journal {
            file,
            tags,
            written: Written::filled(tree.len()),
            sequence,
            heads: HashMap::default(),
            open: Batch::default(),
            flight: Flight::Idle,
            cut: (valid < bytes.len()).then_some(valid as u64),
            // Without the image it starts from, the file is rewritten as what loaded; set
            // below too if a head is dropped.
            compact: lost_base,
            dirty: !bytes.is_empty(),
            appended: valid as u64,
            rewritten: 0,
            rewriting: 0,
            next_ticket: 0,
            base: None,
            staged: None,
            next: None,
            switching: None,
            settled: true,
            rewrite_settles: false,
            // alloc: empty, allocates nothing; grown once and reused by every record.
            stack: Vec::new(),
        };
        match from {
            Some((image, true)) => {
                journal.next = Some(image);
                journal.compact = true;
            }
            Some((image, false)) => journal.base = Some(image),
            None => {}
        }
        let mut heads = HashMap::default();
        // alloc: once per recovery, the heads by name.
        let mut names: Vec<Box<str>> = moves.keys().cloned().collect();
        names.sort_unstable();
        for name in names {
            let mut moves = moves.remove(&name).expect("a head read");
            // The latest record whose root survived; a later one whose nodes did not is dropped.
            moves.sort_unstable();
            while let Some((sequence, root)) = moves.pop() {
                let Some(root) = root else { break };
                let Some(root) = tree.node(&root).filter(|it| tree.is_dir(*it)) else {
                    found.heads_dropped += 1;
                    // Its record stays in the prefix: rewrite the file as the heads are, or it
                    // comes back once a commit makes that root again.
                    journal.compact = true;
                    continue;
                };
                // clone: once per head recovered, its name kept by both maps.
                heads.insert(name.clone(), root);
                journal.heads.insert(name, sequence);
                break;
            }
        }
        Loaded { tree, heads, journal, found }
    }

    /// Records that `head` moved to `root`; the ticket is in what [`Journal::complete`] returns
    /// once that is durable.
    pub(crate) fn record_head(&mut self, tree: &Tree, head: &str, root: NodeId) -> Ticket {
        self.written.grow(tree.len());
        write_node(tree, &mut self.written, &mut self.stack, &mut self.open.bytes, root);
        self.sequence = next(self.sequence);
        match self.heads.get_mut(head) {
            Some(sequence) => *sequence = self.sequence,
            None => {
                self.heads.insert(head.into(), self.sequence);
            }
        }
        head_record(&mut self.open.bytes, self.sequence, Some(&tree.id(root)), head);
        self.ticket()
    }

    /// Records that `head` is gone.
    pub(crate) fn record_removal(&mut self, head: &str) -> Ticket {
        self.sequence = next(self.sequence);
        self.heads.remove(head);
        head_record(&mut self.open.bytes, self.sequence, None, head);
        self.ticket()
    }

    fn ticket(&mut self) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.open.tickets.push(ticket);
        ticket
    }

    /// The sequence number `head` was last written with.
    pub(crate) fn sequence(&self, head: &str) -> Option<u64> {
        self.heads.get(head).copied()
    }

    /// The largest sequence number given out.
    pub(crate) fn last_sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether an image of the tree written now can become the file's base: no image is on
    /// its way to being one, and the file names only nodes the tree has or the file makes, so
    /// it loads whole over that image if the rewrite naming it never happens.
    pub(crate) fn can_stage(&self) -> bool {
        self.next.is_none() && self.switching.is_none() && self.settled
    }

    /// The image the file starts from, which an image written now records.
    pub(crate) fn based_on(&self) -> Option<ContentId> {
        self.base.as_ref().map(|it| it.name)
    }

    /// An image named `name` holding the tree's first `nodes` nodes (all of them now) is being
    /// written, in place of one written before and not yet durable. Only when
    /// [`Journal::can_stage`].
    pub(crate) fn stage(&mut self, name: ContentId, nodes: u64) {
        assert!(self.can_stage(), "an image staged while the journal's base is changing");
        self.staged = Some(FileBase { name, nodes, of: Written::filled(nodes as usize) });
    }

    /// The image named `name` is durable: the next flush rewrites the file to start from it.
    /// `false` for an image not the one last staged, which nothing then uses.
    pub(crate) fn durable(&mut self, name: ContentId) -> bool {
        if self.staged.as_ref().is_none_or(|it| it.name != name) {
            return false;
        }
        self.next = self.staged.take();
        self.compact = true;
        true
    }

    /// Starts the next effect if none is in flight: a rewrite if one was asked for, else the cut
    /// recovery asked for, else an append of what was recorded since the last.
    pub(crate) fn flush(
        &mut self,
        tree: &Tree,
        heads: &HashMap<Box<str>, NodeId>,
        effects: &mut Vec<Effect>,
    ) {
        if !matches!(self.flight, Flight::Idle) {
            return;
        }
        if self.compact {
            self.compact = false;
            // The rewrite replaces the whole file, the torn tail with it.
            self.cut = None;
            self.dirty = false;
            self.appended = 0;
            let target = self.next.take();
            // alloc: the rewrite's bytes, moved into its effect; once per rewrite.
            let mut bytes = Vec::new();
            self.written = match target.as_ref().or(self.base.as_ref()) {
                Some(base) => {
                    frame(&mut bytes, BASE, |out| {
                        out.put_bytes(&base.name.0);
                        out.put_u64(base.nodes);
                    });
                    // clone: one bit per node, once per rewrite.
                    let mut written = base.of.clone();
                    written.grow(tree.len());
                    written
                }
                None => Written::new(tree.len()),
            };
            self.rewrite_settles = true;
            self.switching = target;
            // alloc: once per rewrite, the heads by name.
            let mut names: Vec<&Box<str>> = heads.keys().collect();
            names.sort_unstable();
            for name in &names {
                write_node(tree, &mut self.written, &mut self.stack, &mut bytes, heads[*name]);
            }
            for name in &names {
                let sequence = self.heads[&***name];
                head_record(&mut bytes, sequence, Some(&tree.id(heads[*name])), name);
            }
            frame(&mut bytes, SEQUENCE, |out| out.put_u64(self.sequence));
            // What was recorded and not appended is in the rewrite: the heads as they are now.
            let tickets = std::mem::take(&mut self.open).tickets;
            self.rewriting = bytes.len() as u64;
            // clone: an effect owns its file's name (blaze-core's `Effect`); one per effect.
            effects.push(Effect::Write { file: self.file.clone(), bytes, tag: self.tag(REWRITE) });
            self.flight = Flight::Rewriting(tickets);
            return;
        }
        if let Some(len) = self.cut {
            // clone: an effect owns its file's name (blaze-core's `Effect`); one per effect.
            effects.push(Effect::Truncate { file: self.file.clone(), len, tag: self.tag(CUT) });
            self.flight = Flight::Cutting;
            return;
        }
        if self.open.tickets.is_empty() && self.open.bytes.is_empty() {
            return;
        }
        let Batch { bytes, tickets } = std::mem::take(&mut self.open);
        self.appended += bytes.len() as u64;
        // clone: an effect owns its file's name (blaze-core's `Effect`); one per effect.
        effects.push(Effect::Append { file: self.file.clone(), bytes, tag: self.tag(APPEND) });
        self.dirty = true;
        self.flight = Flight::Appending(tickets);
    }

    fn tag(&self, offset: u64) -> Tag {
        Tag(self.tags + offset)
    }

    pub(crate) fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Takes the completion of one of its effects; returns the tickets now durable.
    pub(crate) fn complete(
        &mut self,
        tree: &Tree,
        heads: &HashMap<Box<str>, NodeId>,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Ticket> {
        let offset = completion.tag.0 - self.tags;
        // alloc: empty, allocates nothing; the tickets of a batch made durable move in.
        let mut durable = Vec::new();
        match (offset, completion.done, std::mem::replace(&mut self.flight, Flight::Idle)) {
            (APPEND, Done::Appended, Flight::Appending(tickets)) => {
                // clone: an effect owns its file's name (blaze-core's `Effect`); one per effect.
                effects.push(Effect::Sync { file: self.file.clone(), tag: self.tag(SYNC) });
                self.flight = Flight::Syncing(tickets);
            }
            (SYNC, Done::Synced, Flight::Syncing(tickets)) => durable = tickets,
            (CUT, Done::Truncated, Flight::Cutting) => {
                self.cut = None;
                // clone: an effect owns its file's name (blaze-core's `Effect`); one per effect.
                effects.push(Effect::Sync { file: self.file.clone(), tag: self.tag(SYNC) });
                // alloc: empty, allocates nothing.
                self.flight = Flight::Syncing(Vec::new());
            }
            (CUT, Done::Failed(_), Flight::Cutting) => {}
            (REWRITE, Done::Written, Flight::Rewriting(tickets)) => {
                if let Some(base) = self.switching.take() {
                    self.base = Some(base);
                }
                self.settled = self.rewrite_settles;
                self.rewritten = self.rewriting;
                durable = tickets;
            }
            // A failed rewrite left the file as it was. A failed append may have left part of
            // its batch, and after a failed sync the kernel may have dropped the pages it could
            // not write, so a second sync could succeed without them. Either way the next
            // effect is a rewrite made of the state then, which holds every move recorded since
            // and replaces whatever the file has.
            (
                APPEND | SYNC | REWRITE,
                Done::Failed(_),
                Flight::Appending(tickets) | Flight::Syncing(tickets) | Flight::Rewriting(tickets),
            ) => {
                let later = std::mem::take(&mut self.open).tickets;
                self.open.tickets = tickets;
                self.open.tickets.extend(later);
                // A rewrite that failed starting from a new image is made again from it.
                if let Some(base) = self.switching.take() {
                    self.next = Some(base);
                }
                self.compact = true;
            }
            (offset, done, _) => {
                unreachable!("the journal has nothing in flight as tag +{offset} for {done:?}")
            }
        }
        self.flush(tree, heads, effects);
        durable
    }

    /// Follows a collection: each node's new number, and a rewrite asked for if the file holds
    /// anything the heads may no longer reach and as many bytes were appended since the last
    /// rewrite as it held. A node of the file's base dropped leaves the file naming a node the
    /// tree no longer has, so then it is rewritten at once, and no image is written until it is.
    pub(crate) fn collected(&mut self, renumbered: &[Option<NodeId>], len: usize) {
        self.written = self::renumbered(&self.written, renumbered, len).0;
        let mut lost = false;
        for base in [&mut self.base, &mut self.switching].into_iter().flatten() {
            let (of, dropped) = self::renumbered(&base.of, renumbered, len);
            base.of = of;
            lost |= dropped;
        }
        for image in [&mut self.staged, &mut self.next].into_iter().flatten() {
            image.of = self::renumbered(&image.of, renumbered, len).0;
        }
        if lost {
            self.settled = false;
            self.rewrite_settles = false;
        }
        self.compact |= lost || (self.dirty && self.appended >= self.rewritten);
    }
}

/// The sequence number after `sequence`, which is below [`SEQUENCES`].
fn next(sequence: u64) -> u64 {
    let next = sequence + 1;
    assert!(next < SEQUENCES, "2^63 head moves in one journal");
    next
}

/// Records `node` and everything under it not recorded yet, children first, so a record only
/// names nodes recorded before it. Each record is framed in place in `out`.
fn write_node(
    tree: &Tree,
    written: &mut Written,
    stack: &mut Vec<(NodeId, bool)>,
    out: &mut Vec<u8>,
    node: NodeId,
) {
    stack.clear();
    stack.push((node, false));
    while let Some((node, children_done)) = stack.pop() {
        if written.contains(node) {
            continue;
        }
        // A file of the image is in the base the file starts from, so its bit is set: no
        // record reads the image's bytes.
        let bytes = tree.bytes(node).unwrap_or_else(|corrupt| {
            panic!("the journal records a file of the image, which its base holds: {corrupt}")
        });
        if let Some(bytes) = bytes {
            frame_file(out, &tree.id(node), bytes);
        } else if !children_done {
            stack.push((node, true));
            let entries = tree.entries(node).expect("a directory");
            stack.extend(entries.iter().filter(|it| !written.contains(it.1)).map(|it| (it.1, false)));
            continue;
        } else {
            let entries = tree.entries(node).expect("a directory");
            frame(out, DIR, |out| {
                for (name, child, mode) in entries.iter() {
                    out.put_bytes_u32(tree.name_str(name).as_bytes());
                    out.put_u32(tree.code(child, mode));
                    out.put_bytes(&tree.id(child).0);
                }
            });
        }
        written.insert(node);
    }
}

fn head_record(out: &mut Vec<u8>, sequence: u64, root: Option<&ContentId>, head: &str) {
    let kind = if root.is_some() { HEAD } else { REMOVE };
    frame(out, kind, |out| {
        out.put_u64(sequence);
        if let Some(root) = root {
            out.put_bytes(&root.0);
        }
        out.put_bytes(head.as_bytes());
    });
}

fn checksum(kind: u8, payload: &[u8]) -> [u8; CHECKSUM] {
    let id = Hasher::new(CHECKSUM_DOMAIN).update(&[kind]).update(payload).finish();
    id.0[..CHECKSUM].try_into().expect("eight bytes")
}

/// Appends one record whose payload `payload` writes, in place: the length is filled in after.
fn frame(out: &mut Vec<u8>, kind: u8, payload: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.put_bytes(&MAGIC);
    out.put_u32(0);
    out.put_u8(kind);
    payload(out);
    let len = u32::try_from(out.len() - start - HEADER).expect("a record under 4 GiB");
    out.patch_u32(start + MAGIC.len(), len);
    let sum = checksum(kind, &out[start + HEADER..]);
    out.put_bytes(&sum);
}

/// Appends the record of a file whose content ID is `id`: its checksum is over the ID, so
/// its bytes are copied and not hashed again.
fn frame_file(out: &mut Vec<u8>, id: &ContentId, bytes: &[u8]) {
    debug_assert_eq!(*id, ContentId::of(FILE_DOMAIN, bytes), "a file record's ID");
    out.reserve(HEADER + bytes.len() + CHECKSUM);
    out.put_bytes(&MAGIC);
    out.put_u32(u32::try_from(bytes.len()).expect("a record under 4 GiB"));
    out.put_u8(FILE);
    out.put_bytes(bytes);
    out.put_bytes(&checksum(FILE, &id.0));
}

/// One record of `kind` holding `payload`, framed with a good checksum: for tests that forge one.
#[cfg(test)]
pub(crate) fn framed(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match kind {
        FILE => frame_file(&mut out, &ContentId::of(FILE_DOMAIN, payload), payload),
        _ => frame(&mut out, kind, |out| out.extend_from_slice(payload)),
    }
    out
}

#[cfg(test)]
pub(crate) const KINDS: (u8, u8, u8, u8, u8) = (FILE, DIR, HEAD, REMOVE, SEQUENCE);

#[cfg(test)]
pub(crate) const BASE_KIND: u8 = BASE;

/// The image a journal's file starts from, if its first record is a base record: the hash of
/// the image's tree and its node count.
pub(crate) fn base_of(bytes: &[u8]) -> Option<(ContentId, u64)> {
    let Record { kind, payload, .. } = record(bytes)?;
    if kind != BASE || payload.len() != BASE_PAYLOAD {
        return None;
    }
    let mut read = Reader::new(payload);
    let name = ContentId(*read.array().ok()?);
    Some((name, read.u64().ok()?))
}

/// A whole record with a good checksum.
struct Record<'a> {
    kind: u8,
    payload: &'a [u8],
    /// Its whole length, framing and all.
    len: usize,
    /// A file record's content ID, which its checksum is over.
    file: Option<ContentId>,
}

/// The record at the start of `bytes`, if it is a whole record with a good checksum.
fn record(bytes: &[u8]) -> Option<Record<'_>> {
    let mut read = Reader::new(bytes);
    if *read.array::<4>().ok()? != MAGIC {
        return None;
    }
    let len = read.u32().ok()? as usize;
    let kind = read.u8().ok()?;
    let payload = read.take(len).ok()?;
    let sum = read.array::<CHECKSUM>().ok()?;
    let file = (kind == FILE).then(|| ContentId::of(FILE_DOMAIN, payload));
    let good = match &file {
        Some(id) => checksum(FILE, &id.0),
        None => checksum(kind, payload),
    };
    (good == *sum).then_some(Record { kind, payload, len: read.at(), file })
}

/// Loads one record; `None` when it names a node that is not there or is malformed (a
/// directory's names not distinct path components in order, a sequence number with no next
/// below [`SEQUENCES`]). `sequence` becomes the largest any record names.
fn load(
    tree: &mut Tree,
    record: &Record<'_>,
    moves: &mut HashMap<Box<str>, Vec<(u64, Option<ContentId>)>>,
    sequence: &mut u64,
) -> Option<()> {
    let payload = record.payload;
    match record.kind {
        FILE => {
            let id = record.file.expect("a file record's ID, computed to check it");
            // alloc: recovery only, a file's bytes out of the journal's, which the caller keeps.
            tree.file_with(id, Bytes::from(Arc::<[u8]>::from(payload)));
        }
        DIR => {
            // alloc: recovery only, one directory record's entries, which `Tree::dir` takes.
            let mut entries = Vec::new();
            let mut rest = Reader::new(payload);
            let mut last: Option<&str> = None;
            while !rest.is_done() {
                let name = rest.str_u32().ok()?;
                // As the tree writes them: one component each, sorted by bytes, so distinct.
                let one = check(name).is_ok() && !name.contains('/');
                if !one || last.is_some_and(|it| it >= name) {
                    return None;
                }
                last = Some(name);
                let code = rest.u32().ok()?;
                let id = ContentId(*rest.array().ok()?);
                let child = tree.node(&id)?;
                // A directory is its node's kind; the code must say the same.
                let mode = match code {
                    DIR_MODE if tree.is_dir(child) => Mode::Regular,
                    _ if tree.is_dir(child) => return None,
                    _ => Mode::from_git(code)?,
                };
                let name = tree.name(name);
                entries.push((name, child, mode));
            }
            tree.dir(entries);
        }
        HEAD => {
            let mut read = Reader::new(payload);
            let at = sequence_at(&mut read)?;
            let root = ContentId(*read.array().ok()?);
            let name = std::str::from_utf8(read.take_rest()).ok()?;
            moves.entry(name.into()).or_default().push((at, Some(root)));
            *sequence = (*sequence).max(at);
        }
        REMOVE => {
            let mut read = Reader::new(payload);
            let at = sequence_at(&mut read)?;
            let name = std::str::from_utf8(read.take_rest()).ok()?;
            moves.entry(name.into()).or_default().push((at, None));
            *sequence = (*sequence).max(at);
        }
        SEQUENCE => {
            let mut read = Reader::new(payload);
            let at = sequence_at(&mut read).filter(|_| read.is_done())?;
            *sequence = (*sequence).max(at);
        }
        _ => return None,
    }
    Some(())
}

/// The sequence number a record's payload starts with, if the one after it is below
/// [`SEQUENCES`], so a head moved after recovery is numbered.
fn sequence_at(read: &mut Reader<'_>) -> Option<u64> {
    read.u64().ok().filter(|&at| at < SEQUENCES - 1)
}
