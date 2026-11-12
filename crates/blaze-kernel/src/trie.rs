//! A map kept in the store as records: 32-byte keys to bytes, as a 16-way trie on the nibbles
//! of the key, each node one write-once record under the hash of its own bytes. What a
//! session's rows and postings are saved as (docs/v2/plan/design/kernel.md, "Carrying a
//! session").
//!
//! - **Nothing of it is held in memory.** [`get`] reads the nodes from a root to one key;
//!   [`update`] reads the paths to the keys it changes, writes those paths anew and gives the
//!   new root. Every node no change is under is shared by the old root and the new.
//! - **A node's bytes are a function of the entries under it** (rule 25): a subtree of at most
//!   [`LEAF`] entries is one leaf, its entries in key order; any other is a branch of its
//!   children in nibble order. Nothing is ever taken out, so an update keeps that form.
//! - **Children are written before their parent** and a root's key is given only after its
//!   put: a save that stops part way leaves nodes no root names, never a root with a node
//!   missing. A node a parent names that the store does not have is an error naming its key
//!   ([`Broken`]), never read as an empty subtree.

use blaze_cas::{Key, Kind};
use blaze_ids::ContentId;

use crate::store::{Get, Put};

/// `blaze kernel t`, then the layout's number.
const MAGIC: [u8; 16] = *b"blaze kernel t 1";
const TAG_LEAF: u8 = 0;
const TAG_BRANCH: u8 = 1;

/// The most entries of a leaf above the last nibble: a row or a posting is a few hundred
/// bytes, so a leaf is a few kilobytes at most and a path is `log16(n / LEAF)` branches.
pub(crate) const LEAF: usize = 8;

/// A key's nibbles: a branch at this depth has nothing to branch on.
const DEEPEST: usize = 64;

/// A node that cannot be read, or a put the store refused: the node's key and why.
pub(crate) struct Broken {
    pub key: Key,
    pub why: String,
}

fn broken(key: Key, why: &str) -> Broken {
    // alloc: an error
    Broken { key, why: why.to_owned() }
}

/// A reader over a record's bytes: none past its end.
pub(crate) struct Cursor<'s> {
    bytes: &'s [u8],
    at: usize,
}

impl<'s> Cursor<'s> {
    pub(crate) fn new(bytes: &'s [u8]) -> Cursor<'s> {
        Cursor { bytes, at: 0 }
    }

    pub(crate) fn take(&mut self, len: usize) -> Option<&'s [u8]> {
        let end = self.at.checked_add(len)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    pub(crate) fn id(&mut self) -> Option<ContentId> {
        Some(ContentId(self.take(32)?.try_into().ok()?))
    }

    /// An ID behind a byte that says whether it is there.
    pub(crate) fn maybe_id(&mut self) -> Option<Option<ContentId>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.id()?)),
            _ => None,
        }
    }

    pub(crate) fn ended(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Appends an ID behind a byte that says whether it is there ([`Cursor::maybe_id`]).
pub(crate) fn put_maybe_id(out: &mut Vec<u8>, id: Option<ContentId>) {
    match id {
        None => out.push(0),
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.0);
        }
    }
}

/// A node as its record says it, read in place.
enum Node<'s> {
    /// Its entries, in key order.
    Leaf(Vec<(ContentId, &'s [u8])>),
    /// Which nibbles have a child, by bit, and the children's keys in nibble order.
    Branch { bitmap: u16, children: &'s [u8] },
}

fn read<'s, G: Get + ?Sized>(store: &'s G, key: Key) -> Result<Node<'s>, Broken> {
    let bytes = match store.get(&key) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(broken(
                key,
                "a node of a session's rows or postings is missing: the node above it names it, so its entries are not known (never read as none)",
            ));
        }
        Err(why) => return Err(Broken { key, why }),
    };
    parse(bytes).ok_or_else(|| match bytes.get(..MAGIC.len()) {
        Some(magic) if magic[..MAGIC.len() - 1] == MAGIC[..MAGIC.len() - 1] => Broken {
            key,
            // alloc: an error
            why: format!(
                "its value is a session node of layout {}, where this blaze reads layout {}",
                magic[MAGIC.len() - 1] as char,
                MAGIC[MAGIC.len() - 1] as char
            ),
        },
        _ => broken(key, "its value is no session node"),
    })
}

fn parse(bytes: &[u8]) -> Option<Node<'_>> {
    let mut input = Cursor::new(bytes);
    if input.take(MAGIC.len())? != MAGIC {
        return None;
    }
    match input.u8()? {
        TAG_LEAF => {
            let count = usize::from(input.u16()?);
            // alloc: one node's entries, for the length of one read of it
            let mut entries = Vec::with_capacity(count.min(LEAF));
            for _ in 0..count {
                let key = input.id()?;
                let len = input.u32()? as usize;
                entries.push((key, input.take(len)?));
            }
            input.ended().then_some(Node::Leaf(entries))
        }
        TAG_BRANCH => {
            let bitmap = input.u16()?;
            let children = input.take(bitmap.count_ones() as usize * 32)?;
            input.ended().then_some(Node::Branch { bitmap, children })
        }
        _ => None,
    }
}

/// Nibble `depth` of `key`, the highest first.
fn nibble(key: &ContentId, depth: usize) -> usize {
    let byte = key.0[depth / 2];
    usize::from(if depth % 2 == 0 { byte >> 4 } else { byte & 15 })
}

/// The key of child `nibble` of a branch, none where its bit is not set.
fn child(bitmap: u16, children: &[u8], nibble: usize) -> Option<Key> {
    if bitmap & (1 << nibble) == 0 {
        return None;
    }
    let before = (bitmap & ((1u16 << nibble) - 1)).count_ones() as usize;
    let key: [u8; 32] = children[before * 32..before * 32 + 32]
        .try_into()
        .expect("a branch holds 32 bytes a set bit");
    Some(Key(ContentId(key)))
}

/// What the map under `root` holds for `key`: one get a node on the way, none where the map
/// has no such key. No root is the empty map.
pub(crate) fn get<G: Get + ?Sized>(
    store: &G,
    root: Option<Key>,
    key: &ContentId,
) -> Result<Option<Vec<u8>>, Broken> {
    let Some(mut at) = root else {
        return Ok(None);
    };
    // Bounded (rule 18): a node a nibble of the key, and a branch past the last is refused.
    for depth in 0..=DEEPEST {
        match read(store, at)? {
            Node::Leaf(entries) => {
                // alloc: the value found, handed to the session that asked
                return Ok(entries.iter().find(|(it, _)| it == key).map(|(_, value)| value.to_vec()));
            }
            Node::Branch { .. } if depth == DEEPEST => break,
            Node::Branch { bitmap, children } => match child(bitmap, children, nibble(key, depth)) {
                Some(next) => at = next,
                None => return Ok(None),
            },
        }
    }
    Err(broken(at, "a branch below a key's last nibble"))
}

/// Every entry of the map under `root`, in key order.
pub(crate) fn each<G: Get + ?Sized>(
    store: &G,
    root: Option<Key>,
    visit: &mut dyn FnMut(&ContentId, &[u8]),
) -> Result<(), Broken> {
    match root {
        Some(root) => each_under(store, root, 0, visit),
        None => Ok(()),
    }
}

fn each_under<G: Get + ?Sized>(
    store: &G,
    at: Key,
    depth: usize,
    visit: &mut dyn FnMut(&ContentId, &[u8]),
) -> Result<(), Broken> {
    match read(store, at)? {
        Node::Leaf(entries) => {
            for (key, value) in &entries {
                visit(key, value);
            }
            Ok(())
        }
        Node::Branch { .. } if depth == DEEPEST => {
            Err(broken(at, "a branch below a key's last nibble"))
        }
        Node::Branch { bitmap, children } => {
            for n in 0..16 {
                if let Some(next) = child(bitmap, children, n) {
                    each_under(store, next, depth + 1, visit)?;
                }
            }
            Ok(())
        }
    }
}

/// The map under `root` with each of `changes` put in it (a key the map has takes the new
/// bytes): the new root. Reads the nodes on the way to each changed key and writes each of
/// them once, children first; every other node is the old map's. `changes` names a key once.
pub(crate) fn update<P: Put + ?Sized>(
    store: &mut P,
    root: Option<Key>,
    mut changes: Vec<(ContentId, Vec<u8>)>,
) -> Result<Option<Key>, Broken> {
    if changes.is_empty() {
        return Ok(root);
    }
    changes.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    under(store, root, changes, 0).map(Some)
}

/// What a node is before it is changed: owned, so the store can be written.
enum Had {
    Entries(Vec<(ContentId, Vec<u8>)>),
    Children([Option<Key>; 16]),
}

/// The node at `at` with `changes` (in key order, not empty, all under it) put in it: its key.
fn under<P: Put + ?Sized>(
    store: &mut P,
    at: Option<Key>,
    changes: Vec<(ContentId, Vec<u8>)>,
    depth: usize,
) -> Result<Key, Broken> {
    // What the node was, and the changes still to put under its children.
    let (had, changes) = match at {
        None => (Had::Entries(changes), Vec::new()),
        Some(at) => match read(&*store, at)? {
            Node::Leaf(entries) => {
                // alloc: one leaf's entries with the changed ones, until it is written
                let mut merged = Vec::with_capacity(entries.len() + changes.len());
                let mut old = entries.into_iter().peekable();
                for change in changes {
                    while let Some((key, value)) = old.next_if(|(key, _)| key.0 < change.0.0) {
                        // alloc: an entry of a leaf that is written again
                        merged.push((key, value.to_vec()));
                    }
                    let _ = old.next_if(|(key, _)| key.0 == change.0.0);
                    merged.push(change);
                }
                // alloc: an entry of a leaf that is written again
                merged.extend(old.map(|(key, value)| (key, value.to_vec())));
                // alloc: none: an empty list
                (Had::Entries(merged), Vec::new())
            }
            Node::Branch { .. } if depth == DEEPEST => {
                return Err(broken(at, "a branch below a key's last nibble"));
            }
            Node::Branch { bitmap, children } => {
                let mut keys = [None; 16];
                for (n, key) in keys.iter_mut().enumerate() {
                    *key = child(bitmap, children, n);
                }
                (Had::Children(keys), changes)
            }
        },
    };
    match had {
        Had::Entries(entries) => pack(store, entries, depth),
        Had::Children(mut keys) => {
            let mut changes = changes.into_iter().peekable();
            for (n, key) in keys.iter_mut().enumerate() {
                // alloc: the changes under one child
                let mut below = Vec::new();
                while let Some(change) = changes.next_if(|(it, _)| nibble(it, depth) == n) {
                    below.push(change);
                }
                if !below.is_empty() {
                    *key = Some(under(store, *key, below, depth + 1)?);
                }
            }
            branch(store, &keys)
        }
    }
}

/// `entries` (in key order, all under one node at `depth`) as a subtree: its key.
fn pack<P: Put + ?Sized>(
    store: &mut P,
    entries: Vec<(ContentId, Vec<u8>)>,
    depth: usize,
) -> Result<Key, Broken> {
    if entries.len() <= LEAF || depth == DEEPEST {
        // alloc: one node's record
        let mut bytes = Vec::with_capacity(
            MAGIC.len() + 3 + entries.iter().map(|(_, value)| 36 + value.len()).sum::<usize>(),
        );
        bytes.extend_from_slice(&MAGIC);
        bytes.push(TAG_LEAF);
        let count = u16::try_from(entries.len()).expect("a leaf holds a few entries");
        bytes.extend_from_slice(&count.to_be_bytes());
        for (key, value) in &entries {
            bytes.extend_from_slice(&key.0);
            let len = u32::try_from(value.len()).expect("a row or a posting is counted in u32");
            bytes.extend_from_slice(&len.to_be_bytes());
            bytes.extend_from_slice(value);
        }
        return write(store, bytes);
    }
    let mut keys = [None; 16];
    let mut entries = entries.into_iter().peekable();
    for (n, key) in keys.iter_mut().enumerate() {
        // alloc: the entries under one child
        let mut below = Vec::new();
        while let Some(entry) = entries.next_if(|(it, _)| nibble(it, depth) == n) {
            below.push(entry);
        }
        if !below.is_empty() {
            *key = Some(pack(store, below, depth + 1)?);
        }
    }
    branch(store, &keys)
}

fn branch<P: Put + ?Sized>(store: &mut P, keys: &[Option<Key>; 16]) -> Result<Key, Broken> {
    // alloc: one node's record
    let mut bytes = Vec::with_capacity(MAGIC.len() + 3 + 16 * 32);
    bytes.extend_from_slice(&MAGIC);
    bytes.push(TAG_BRANCH);
    let mut bitmap = 0u16;
    for (n, key) in keys.iter().enumerate() {
        if key.is_some() {
            bitmap |= 1 << n;
        }
    }
    bytes.extend_from_slice(&bitmap.to_be_bytes());
    for key in keys.iter().flatten() {
        bytes.extend_from_slice(&key.0.0);
    }
    write(store, bytes)
}

/// Puts a node under the hash of its bytes.
fn write<P: Put + ?Sized>(store: &mut P, bytes: Vec<u8>) -> Result<Key, Broken> {
    let key = Key(ContentId::of("blaze v2 kernel session node 1", &bytes));
    match store.put(key, Kind::Query, bytes) {
        Ok(()) => Ok(key),
        Err(mismatch) => Err(Broken {
            key,
            // alloc: an error
            why: format!(
                "the store keeps other bytes ({:x}) under the hash of a session node ({:x})",
                mismatch.kept, mismatch.given
            ),
        }),
    }
}
