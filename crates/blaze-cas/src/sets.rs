//! Item sets, the live form (docs/v2/plan/design/store.md, "Item sets: one value an item").
//!
//! The front end keeps one value an item, each under a key of the item's own content and
//! what it read. Here each is a record under that key, as any value is, so an unchanged item
//! of an edited file, or of a crate's next version, is the same record, found by its key
//! under any set. What a set adds is a root to keep its items by: a *listing*, a record of
//! [`Kind::Set`] that names other records and that the collector follows as it follows a
//! chunked output to its chunks (`collect::named_by`). A pin or a keep names a set's root
//! alone; adding an item rewrites no pin.
//!
//! A set's listing is two levels. A *leaf* lists item keys in order; where one leaf ends and
//! the next begins is decided by the keys themselves ([`ends_leaf`]: one key in
//! [`LEAF_EVERY`] ends a leaf), so adding or dropping an item changes the one leaf it falls
//! in and no other, and two sets that hold the same run of items share the leaf's record. The
//! *root* lists the set's leaves (and, for the sealed form to come, the sealed sets its
//! crate's earlier versions were compacted to). Every listing is keyed by its own bytes:
//! equal listings are one record, and a listing never changes under its key.
//!
//! The invariant: a root [`Sets::list`] returned names, through its leaves, every item got or
//! put under its set since the daemon started; so while a root is kept (a pin names it, a
//! reader keeps it), the collector forgets none of them. Tested in `tests::sets` (the
//! collector's simulation with a pinned root; the control is a collector that does not
//! follow listings, `collect::Plant::SetNotFollowed`).

use std::{alloc::Global, sync::Arc};

use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::{Handle, Key, Kind};

/// The first bytes of a listing: its layout's version is the last.
const MAGIC: [u8; 8] = *b"BLZSET\x00\x01";
const LEAF_DOMAIN: &str = "blaze v2 store set leaf 1";
const ROOT_DOMAIN: &str = "blaze v2 store set root 1";
/// One key in this many ends a leaf: a leaf lists about this many items.
pub const LEAF_EVERY: u16 = 128;
/// A leaf's bytes before its keys: the magic, the level and padding, the count.
const LEAF_FIXED: usize = 8 + 4 + 4;
/// A root's bytes before its keys: as a leaf's, the count of sealed sets, the set.
const ROOT_FIXED: usize = 8 + 4 + 4 + 4 + 32;
/// The locks the sets' members are spread over, by set.
const SHARDS: usize = 64;

/// Whether `key` is the last of its leaf: decided by the key alone, so the leaves of a set
/// depend only on which keys it holds.
pub fn ends_leaf(key: &Key) -> bool {
    u16::from_le_bytes([key.0.0[0], key.0.0[1]]) % LEAF_EVERY == 0
}

/// A listing read: what it names inside the store.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Listing {
    /// Item keys, ascending.
    Leaf(Vec<Key>),
    /// A set's leaves in order, and the sealed sets it names.
    Root { set: ContentId, leaves: Vec<Key>, sealed: Vec<Key> },
}

impl Listing {
    /// Every key it names.
    pub fn named(&self) -> impl Iterator<Item = Key> + '_ {
        let (first, second): (&[Key], &[Key]) = match self {
            Listing::Leaf(keys) => (keys, &[]),
            Listing::Root { leaves, sealed, .. } => (leaves, sealed),
        };
        first.iter().chain(second).copied()
    }
}

fn push_keys(out: &mut Vec<u8>, keys: &[Key]) {
    for key in keys {
        out.extend_from_slice(&key.0.0);
    }
}

/// A leaf's bytes: `keys` must be ascending and without repeats.
pub fn encode_leaf(keys: &[Key]) -> Vec<u8> {
    debug_assert!(keys.windows(2).all(|it| it[0] < it[1]), "a leaf's keys ascend");
    // alloc: once a leaf written
    let mut out = Vec::with_capacity(LEAF_FIXED + keys.len() * 32);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    push_keys(&mut out, keys);
    out
}

/// A root's bytes.
pub fn encode_root(set: &ContentId, leaves: &[Key], sealed: &[Key]) -> Vec<u8> {
    // alloc: once a root written
    let mut out = Vec::with_capacity(ROOT_FIXED + (leaves.len() + sealed.len()) * 32);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
    out.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
    out.extend_from_slice(&set.0);
    push_keys(&mut out, leaves);
    push_keys(&mut out, sealed);
    out
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

fn keys_at(bytes: &[u8]) -> Vec<Key> {
    bytes
        .chunks_exact(32)
        .map(|it| Key(ContentId(it.try_into().expect("32 bytes a key"))))
        // alloc: once a listing read
        .collect()
}

/// Reads a listing: every byte accounted for, or why not. A listing of another layout (its
/// magic's version is not this one's) is refused by name, never read as empty.
pub fn decode(bytes: &[u8]) -> Result<Listing, String> {
    if bytes.len() < LEAF_FIXED {
        // alloc: error path
        return Err(format!("{} bytes, under a listing's header", bytes.len()));
    }
    if bytes[..7] != MAGIC[..7] {
        return Err("not a listing: its first bytes are not a listing's".into()); // alloc: error path
    }
    if bytes[7] != MAGIC[7] {
        // alloc: error path
        return Err(format!(
            "a listing of layout {}, and this blaze reads layout {}",
            bytes[7], MAGIC[7]
        ));
    }
    if bytes[9..12] != [0, 0, 0] {
        return Err("its padding is not zero".into()); // alloc: error path
    }
    let count = u32_at(bytes, 12);
    match bytes[8] {
        0 => {
            if bytes.len() != LEAF_FIXED + count * 32 {
                // alloc: error path
                return Err(format!("a leaf of {count} keys in {} bytes", bytes.len()));
            }
            let keys = keys_at(&bytes[LEAF_FIXED..]);
            if !keys.windows(2).all(|it| it[0] < it[1]) {
                return Err("a leaf whose keys do not ascend".into()); // alloc: error path
            }
            Ok(Listing::Leaf(keys))
        }
        1 => {
            if bytes.len() < ROOT_FIXED {
                // alloc: error path
                return Err(format!("{} bytes, under a root's header", bytes.len()));
            }
            let sealed = u32_at(bytes, 16);
            if bytes.len() != ROOT_FIXED + (count + sealed) * 32 {
                // alloc: error path
                return Err(format!(
                    "a root of {count} leaves and {sealed} sealed sets in {} bytes",
                    bytes.len()
                ));
            }
            let set = ContentId(bytes[20..52].try_into().expect("32 bytes"));
            let keys = &bytes[ROOT_FIXED..];
            Ok(Listing::Root {
                set,
                leaves: keys_at(&keys[..count * 32]),
                sealed: keys_at(&keys[count * 32..]),
            })
        }
        level => Err(format!("a listing of level {level}")), // alloc: error path
    }
}

/// The listing of a set that holds `keys` (ascending, no repeats): each leaf's key and
/// bytes in order, then the root's. Depends on nothing but `set` and `keys`.
pub fn listing_of(set: &ContentId, keys: &[Key], sealed: &[Key]) -> (Vec<(Key, Vec<u8>)>, (Key, Vec<u8>)) {
    // alloc: once a set listed
    let mut leaves = Vec::new();
    let mut from = 0;
    for (at, key) in keys.iter().enumerate() {
        if ends_leaf(key) || at + 1 == keys.len() {
            let bytes = encode_leaf(&keys[from..=at]);
            leaves.push((Key(ContentId::of(LEAF_DOMAIN, &bytes)), bytes));
            from = at + 1;
        }
    }
    // alloc: once a set listed, a key a leaf
    let leaf_keys: Vec<Key> = leaves.iter().map(|it| it.0).collect();
    let root = encode_root(set, &leaf_keys, sealed);
    (leaves, (Key(ContentId::of(ROOT_DOMAIN, &root)), root))
}

/// One set's members, as gets and puts named them.
#[derive(Default)]
struct Members {
    /// The first `listed` are ascending without repeats: what its last listing named.
    keys: Vec<Key>,
    listed: usize,
    /// Got or put since the last [`Sets::list`].
    touched: bool,
    root: Option<Key>,
    /// The keys of its last listing's leaves, ascending: a leaf among them is in the store.
    leaves: Vec<Key>,
}

/// What [`Sets::list`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Listed {
    /// Sets got or put since the last call: each has a root in the list returned.
    pub sets: u64,
    /// Of those, the sets whose members changed: their listings were put.
    pub changed: u64,
    /// Listings put (roots, and the leaves the set's listing before did not have), and
    /// their bytes.
    pub listings: u64,
    pub bytes: u64,
}

/// The live sets of a daemon: the one `put` and `get` of items ([`Sets::put`], [`Sets::get`];
/// the front end's `hir_def::nameres::frozen::entries::Entries` is these two), and the
/// listings that keep them ([`Sets::list`]). Shared by every worker.
pub struct Sets {
    // alloc: the daemon's own tables, for its life
    shards: Vec<Lock<HashMap<ContentId, Members, Global>>>,
}

impl Sets {
    pub fn new() -> Arc<Sets> {
        // alloc: the daemon's own tables, for its life
        Arc::new(Sets {
            shards: (0..SHARDS)
                .map(|_| Lock::new("store.sets.members", HashMap::new_in(Global)))
                // alloc: the daemon's own tables, for its life
                .collect(),
        })
    }

    fn member(&self, set: &ContentId, key: Key) {
        let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
        let members = shard.entry(*set).or_default();
        members.touched = true;
        // The last one again is the common repeat (a get, then its put): not kept twice.
        if members.keys.last() != Some(&key) {
            // alloc: a key a member got or put, until the set is listed
            members.keys.push(key);
        }
    }

    /// Calls `with` once with the bytes kept under `key` and says `true`; `false`, with no
    /// call, when nothing is kept under it; `Err` when bytes are kept and do not read. The
    /// item is found by `key` alone, whatever `set` it was put under; found, it is a member
    /// of `set` from here on.
    // dyn: as the front end's `Entries::get`, one call an entry read
    pub fn get(
        &self,
        handle: &Handle,
        set: &ContentId,
        key: &ContentId,
        with: &mut dyn FnMut(&[u8]),
    ) -> Result<bool, String> {
        let found = handle
            .read(&Key(*key), |entry| with(entry.bytes))
            // alloc: error path
            .map_err(|corrupt| format!("the item {key:?} of the set {set:?} does not read: {corrupt}"))?
            .is_some();
        if found {
            self.member(set, Key(*key));
        }
        Ok(found)
    }

    /// Keeps `bytes` under `key`, a member of `set`. A key the store has is the same bytes
    /// (a key covers every input of its item) and writes nothing; other bytes under it are
    /// refused, naming the key (rule keys-cover-every-input).
    pub fn put(&self, handle: &Handle, set: &ContentId, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        handle
            .put(Key(key), Kind::Query, bytes)
            // alloc: error path
            .map_err(|mismatch| format!("the item {key:?} of the set {set:?} was refused: {mismatch:?}"))?;
        self.member(set, Key(key));
        Ok(())
    }

    /// The root of `set` as its last listing ([`Sets::list`]) put it: what a pin or a keep
    /// of the set names. `None` for a set never listed.
    pub fn root(&self, set: &ContentId) -> Option<Key> {
        self.shards[set.0[0] as usize % SHARDS].lock().get(set).and_then(|it| it.root)
    }

    /// Lists every set got or put since the last call: a set whose members changed has its
    /// leaves and root put (the store writes only those it lacks: one leaf and the root for
    /// one item more), and each such set's root key is returned, in the order of the sets
    /// (rule deterministic-output), for a pin or a keep to name. Not for the daemon's loop:
    /// it sorts each changed set's keys.
    pub fn list(&self, handle: &Handle) -> Result<(Vec<(ContentId, Key)>, Listed), String> {
        // alloc: once a build's end, a root a set it touched
        let mut roots = Vec::new();
        let mut listed = Listed::default();
        // The sets touched, in their order: what is put is put in that order, whatever the
        // tables' own.
        // alloc: once a build's end, the sets it touched
        let mut touched: Vec<ContentId> = Vec::new();
        for shard in &self.shards {
            touched.extend(shard.lock().iter().filter(|(_, it)| it.touched).map(|(set, _)| *set));
        }
        touched.sort_unstable();
        for set in &touched {
            let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
            let Some(members) = shard.get_mut(set) else { continue };
            members.touched = false;
            listed.sets += 1;
            members.keys.sort_unstable();
            members.keys.dedup();
            let changed = members.keys.len() != members.listed || members.root.is_none();
            if changed {
                listed.changed += 1;
                let (leaves, root) = listing_of(set, &members.keys, &[]);
                for (key, bytes) in leaves.iter().chain([&root]) {
                    // A leaf of the listing before is the same record: not put again.
                    if members.leaves.binary_search(key).is_ok() {
                        continue;
                    }
                    handle
                        .put(*key, Kind::Set, bytes)
                        // alloc: error path
                        .map_err(|mismatch| format!("a listing of the set {set:?} was refused: {mismatch:?}"))?;
                    listed.listings += 1;
                    listed.bytes += bytes.len() as u64;
                }
                members.listed = members.keys.len();
                members.root = Some(root.0);
                // alloc: once a set listed, a key a leaf
                members.leaves = leaves.iter().map(|it| it.0).collect();
                members.leaves.sort_unstable();
            }
            roots.push((*set, members.root.expect("a set listed has a root")));
        }
        Ok((roots, listed))
    }
}
