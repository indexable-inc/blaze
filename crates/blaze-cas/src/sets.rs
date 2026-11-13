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
//! follow listings, `collect::SET_NOT_FOLLOWED`).
//!
//! The sealed form ([`crate::sealed`]) is beside it: a set's items in one value, which the
//! set's root names. A set the driver says is written sealed at once
//! ([`Sets::seal_at_once`]: a crate not edited under its version) holds what is put under
//! it in memory until [`Sets::seal`] writes them as one sealed set; a live set that has
//! been quiet ([`Sets::quiet`]) is compacted by the same [`Sets::seal`], its records left
//! to the collector once no root names them. A get looks at the record under the key, then
//! at what its set holds in memory, then in the sealed sets its set's root names
//! ([`Sets::follows`] gives a crate's next version the sealed sets of the one before).
//!
//! The invariant of a seal: the root [`Sets::seal`] returns names, through its sealed sets
//! and its leaves, every item the root before it named and every item put under the set
//! before the seal began; an item is in a record a root names or in a sealed set a root
//! names at every point, never in neither. Tested in `tests::sealed` (a crash at every
//! completion of the seal's commit and of the pin's; the control is a root that leaves its
//! sealed set out, [`SEALED_NOT_NAMED`]).

use std::{alloc::Global, sync::Arc};

use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::{Handle, Key, Kind, Mismatch, Store, Ticket, Tickets, enter, sealed};

#[cfg(test)]
thread_local! {
    /// A planted bug for the seal's test: the root a seal writes leaves out the sealed set
    /// it just made.
    pub(crate) static SEALED_NOT_NAMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// A set has at most this many sealed sets: a seal that would make one more merges them all
/// into the one it writes, so a get that misses reads this many directories at most.
pub const MAX_SEALED: usize = 4;

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
    /// The sealed sets its root names, oldest first, each with its header as read once.
    sealed: Vec<(Key, sealed::Header)>,
    /// `Some` for a set written sealed at once: what was put under it and is in no sealed
    /// set yet, by key.
    held: Option<HashMap<Key, Vec<u8>, Global>>,
    /// A root adopted ([`Sets::adopt`]) whose leaves are not read into `keys` yet.
    base: Option<Key>,
    /// How many listings in a row found its members as they were.
    quiet: u32,
}

/// What [`Sets::seal`] wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sealing {
    /// The set's root from here on: what a pin or a keep names once `durable` is.
    pub root: Key,
    /// The sealed set written.
    pub sealed: Key,
    /// The items in it, and the bytes of the sealed set.
    pub items: u64,
    pub bytes: u64,
    /// Of the items, those that came from the set's earlier sealed sets, merged into this.
    pub merged: u64,
    /// Members whose record the store no longer has (forgotten while no root named them):
    /// not in the sealed set.
    pub gone: u64,
    /// The later of the two puts (the sealed set, then the root): both are the handle's
    /// one writer's, so the later is durable when both are.
    pub durable: Ticket,
}

/// A sealed set in the store, read a part at a time under a task's guard.
struct InStore<'g> {
    handle: &'g Handle,
    guard: &'g crate::Guard,
    key: Key,
}

impl sealed::Parts for InStore<'_> {
    fn part(&self, at: u64, len: u64) -> Result<&[u8], String> {
        match self.handle.reader().part(self.guard, &self.key, at, len) {
            Ok(Some(bytes)) => Ok(bytes),
            // alloc: error path
            Ok(None) => Err(format!("the sealed set {:?} is not in the store", self.key.0)),
            Err(corrupt) => Err(corrupt.to_string()), // alloc: error path
        }
    }
}

/// The header of the sealed set under `key`, read from the store.
fn header_of(handle: &Handle, key: Key) -> Result<sealed::Header, String> {
    use sealed::Parts as _;
    let guard = enter();
    let from = InStore { handle, guard: &guard, key };
    sealed::Header::decode(from.part(0, sealed::FIXED as u64)?)
        // alloc: error path
        .map_err(|why| format!("the sealed set {:?}: {why}", key.0))
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
    /// Held by a listing and by a seal from end to end: one of them at a time.
    listing: Lock<()>,
}

/// The item keys the leaves of `root` list: what a root an earlier run wrote named.
fn keys_of_root(handle: &Handle, root: Key) -> Result<Vec<Key>, String> {
    let read = |key: Key| {
        handle
            .read(&key, |entry| decode(entry.bytes))
            // alloc: error path
            .map_err(|corrupt| format!("the listing {:?} does not read: {corrupt}", key.0))?
            // alloc: error path
            .ok_or_else(|| format!("the listing {:?} is not in the store, and a root names it", key.0))?
            // alloc: error path
            .map_err(|why| format!("the listing {:?}: {why}", key.0))
    };
    let Listing::Root { leaves, .. } = read(root)? else {
        // alloc: error path
        return Err(format!("{:?} was adopted as a root and is a leaf", root.0));
    };
    // alloc: once a set adopted and then touched, its keys
    let mut keys = Vec::new();
    for leaf in leaves {
        match read(leaf)? {
            Listing::Leaf(named) => keys.extend(named),
            // alloc: error path
            Listing::Root { .. } => return Err(format!("the root {:?} names the root {:?} as a leaf", root.0, leaf.0)),
        }
    }
    Ok(keys)
}

impl Sets {
    pub fn new() -> Arc<Sets> {
        // alloc: the daemon's own tables, for its life
        Arc::new(Sets {
            shards: (0..SHARDS)
                .map(|_| Lock::new("store.sets.members", HashMap::new_in(Global)))
                // alloc: the daemon's own tables, for its life
                .collect(),
            listing: Lock::new("store.sets.listing", ()),
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
            return Ok(true);
        }
        // Not a record: held for a seal, or in a sealed set of this set.
        let sealed_sets = {
            let shard = self.shards[set.0[0] as usize % SHARDS].lock();
            let Some(members) = shard.get(set) else { return Ok(false) };
            if let Some(bytes) = members.held.as_ref().and_then(|held| held.get(&Key(*key))) {
                with(bytes);
                return Ok(true);
            }
            if members.sealed.is_empty() {
                return Ok(false);
            }
            // alloc: a get that missed its record, the set's few sealed sets (MAX_SEALED)
            members.sealed.clone()
        };
        let guard = enter();
        // The newest first: what was sealed last is what was put last.
        for (sealed_key, header) in sealed_sets.iter().rev() {
            let from = InStore { handle, guard: &guard, key: *sealed_key };
            let mut probe = sealed::Probe::default();
            let item = sealed::find(&from, header, &Key(*key), &mut probe)
                // alloc: error path
                .map_err(|why| format!("the item {key:?} of the set {set:?} in the sealed set {:?}: {why}", sealed_key.0))?;
            if let Some(bytes) = item {
                with(bytes);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Says `set` is written sealed at once: what is put under it from here on is held in
    /// memory, readable by [`Sets::get`] under this set, and written by [`Sets::seal`] as
    /// one sealed set, never as a record each. For a crate that is not edited under its
    /// version (the standard library, a registry crate); such an item is not found under
    /// another set, which is the sharing the sealed form gives up.
    pub fn seal_at_once(&self, set: &ContentId) {
        let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
        let members = shard.entry(*set).or_default();
        if members.held.is_none() {
            members.held = Some(HashMap::new_in(Global));
        }
    }

    /// Gives `set` the sealed sets of `before`, the set of its crate's version before: an
    /// unchanged item of the new version is found there, and only what changed is written.
    /// The next listing or seal of `set` names them in its root.
    pub fn follows(&self, set: &ContentId, before: &ContentId) {
        // alloc: once a crate's new version, the few sealed sets of the one before
        let theirs = self.shards[before.0[0] as usize % SHARDS]
            .lock()
            .get(before)
            .map_or_else(Vec::new, |it| it.sealed.clone());
        if theirs.is_empty() {
            return;
        }
        let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
        let members = shard.entry(*set).or_default();
        for it in theirs {
            if !members.sealed.iter().any(|mine| mine.0 == it.0) {
                // alloc: as above
                members.sealed.push(it);
                members.touched = true;
                // Its root does not name them yet: the next listing writes one that does.
                members.root = None;
            }
        }
    }

    /// Takes `root`, a set's root an earlier run of the daemon wrote (a pin names it, or a
    /// build's record), as the set's: its sealed sets are looked in by [`Sets::get`], and
    /// the next listing of the set names what it named and what was got or put since. The
    /// set it is of is returned. A set that has a root already keeps it. `None` for a key
    /// that is no root (a pin names other records too: no value, a value that is not a
    /// listing, a leaf); a listing of another layout or a cut one is an error.
    pub fn adopt(&self, handle: &Handle, root: Key) -> Result<Option<ContentId>, String> {
        let listing = handle
            .read(&root, |entry| {
                (entry.bytes.len() >= 7 && entry.bytes[..7] == MAGIC[..7]).then(|| decode(entry.bytes))
            })
            // alloc: error path
            .map_err(|corrupt| format!("the root {:?} does not read: {corrupt}", root.0))?;
        let Some(Some(listing)) = listing else { return Ok(None) };
        // alloc: error path
        let listing = listing.map_err(|why| format!("the root {:?}: {why}", root.0))?;
        let Listing::Root { set, leaves, sealed: sealed_keys } = listing else { return Ok(None) };
        // alloc: once a root adopted, its few sealed sets
        let mut sealed_sets = Vec::with_capacity(sealed_keys.len());
        for key in sealed_keys {
            sealed_sets.push((key, header_of(handle, key)?));
        }
        let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
        let members = shard.entry(set).or_default();
        if members.root.is_none() && members.base.is_none() {
            members.root = Some(root);
            members.base = Some(root);
            members.leaves = leaves;
            members.leaves.sort_unstable();
            for it in sealed_sets {
                if !members.sealed.iter().any(|mine| mine.0 == it.0) {
                    members.sealed.push(it); // alloc: as above
                }
            }
        }
        Ok(Some(set))
    }

    /// Keeps `bytes` under `key`, a member of `set`. A key the store has is the same bytes
    /// (a key covers every input of its item) and writes nothing; other bytes under it are
    /// refused, naming the key (rule keys-cover-every-input).
    pub fn put(&self, handle: &Handle, set: &ContentId, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        {
            let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
            if let Some(members) = shard.get_mut(set)
                && let Some(held) = members.held.as_mut()
            {
                // A set written sealed at once: held until its seal.
                members.touched = true;
                return match held.get(&Key(key)) {
                    Some(kept) if kept.as_slice() == bytes => Ok(()),
                    // alloc: error path
                    Some(kept) => Err(format!(
                        "the item {key:?} of the set {set:?} was refused: {} bytes are held under it and {} other bytes were put",
                        kept.len(),
                        bytes.len()
                    )),
                    None => {
                        // alloc: an item held until its set is sealed (`Sets::seal`)
                        held.insert(Key(key), bytes.to_vec());
                        Ok(())
                    }
                };
            }
        }
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
        let _one = self.listing.lock();
        for shard in &self.shards {
            touched.extend(shard.lock().iter().filter(|(_, it)| it.touched).map(|(set, _)| *set));
        }
        touched.sort_unstable();
        for set in &touched {
            let mut shard = self.shards[set.0[0] as usize % SHARDS].lock();
            let Some(members) = shard.get_mut(set) else { continue };
            members.touched = false;
            listed.sets += 1;
            // A member that is an item of one of the set's sealed sets (its record was
            // still there to be got) is kept by the sealed set: not listed as a record.
            if !members.sealed.is_empty() && members.keys.len() > members.listed {
                let guard = enter();
                // alloc: once a listing of a set that has sealed sets, its new members
                let mut fresh = members.keys.split_off(members.listed);
                let mut failed = None;
                fresh.retain(|key| {
                    members.sealed.iter().all(|(sealed_key, header)| {
                        let from = InStore { handle, guard: &guard, key: *sealed_key };
                        match sealed::find(&from, header, key, &mut sealed::Probe::default()) {
                            Ok(found) => found.is_none(),
                            Err(why) => {
                                // alloc: error path
                                failed = Some(format!("the sealed set {:?} of the set {set:?}: {why}", sealed_key.0));
                                true
                            }
                        }
                    })
                });
                members.keys.append(&mut fresh);
                if let Some(why) = failed {
                    return Err(why);
                }
            }
            // What an adopted root named is the set's too: read once, here.
            if let Some(base) = members.base {
                let mut named = keys_of_root(handle, base)?;
                named.sort_unstable();
                named.dedup();
                members.listed = named.len();
                members.keys.append(&mut named);
                members.base = None;
            }
            members.keys.sort_unstable();
            members.keys.dedup();
            let changed = members.keys.len() != members.listed || members.root.is_none();
            members.quiet = if changed { 0 } else { members.quiet.saturating_add(1) };
            if changed {
                listed.changed += 1;
                // alloc: once a set listed, its few sealed sets
                let sealed_keys: Vec<Key> = members.sealed.iter().map(|it| it.0).collect();
                let (leaves, root) = listing_of(set, &members.keys, &sealed_keys);
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

    /// The live sets a seal is due for: at least `items` members, and as they were at each
    /// of the last `listings` listings. In the order of the sets.
    pub fn quiet(&self, listings: u32, items: usize) -> Vec<ContentId> {
        // alloc: once a build's end, the sets due
        let mut due = Vec::new();
        for shard in &self.shards {
            due.extend(
                shard
                    .lock()
                    .iter()
                    .filter(|(_, it)| it.held.is_none() && it.quiet >= listings && it.keys.len() >= items)
                    .map(|(set, _)| *set),
            );
        }
        due.sort_unstable();
        due
    }

    /// Writes what `set` holds as one sealed set and a root that names it: the items held
    /// for it (a set written sealed at once) and its members' records (a live set that has
    /// been quiet: its compaction), with the items of its earlier sealed sets when there
    /// are [`MAX_SEALED`] of them. `None` when it holds nothing to seal. Not for the
    /// daemon's loop: it reads every item and hashes the sealed set.
    ///
    /// Nothing is dropped here. The records sealed stay until the collector forgets them,
    /// which it does only once no kept root names them: the caller moves its pin or keep
    /// to [`Sealing::root`] when [`Sealing::durable`] is, and until then the root before
    /// keeps them.
    pub fn seal(&self, handle: &Handle, set: &ContentId) -> Result<Option<Sealing>, String> {
        let _one = self.listing.lock();
        let shard_of = || self.shards[set.0[0] as usize % SHARDS].lock();
        // What the set holds now: sealed from this, whatever is put while the seal runs.
        let (mut keys, held, earlier, base) = {
            let shard = shard_of();
            let Some(members) = shard.get(set) else { return Ok(None) };
            // alloc: once a seal, the set's keys and what is held for it
            let held: Vec<(Key, Vec<u8>)> = members
                .held
                .as_ref()
                .map_or_else(Vec::new, |held| held.iter().map(|(key, bytes)| (*key, bytes.clone())).collect());
            (members.keys.clone(), held, members.sealed.clone(), members.base)
        };
        if let Some(base) = base {
            keys.extend(keys_of_root(handle, base)?);
        }
        keys.sort_unstable();
        keys.dedup();
        // alloc: once a seal, the items' bytes until the sealed set is put
        let mut items: Vec<(Key, Vec<u8>)> = held;
        // alloc: once a seal, the keys held
        let mut held_keys: Vec<Key> = items.iter().map(|it| it.0).collect();
        held_keys.sort_unstable();
        let mut gone = 0;
        for key in &keys {
            let read = handle
                .read(key, |entry| entry.bytes.to_vec()) // alloc: as `items`
                // alloc: error path
                .map_err(|corrupt| format!("the item {:?} of the set {set:?} does not read: {corrupt}", key.0))?;
            match read {
                Some(bytes) => items.push((*key, bytes)),
                None => gone += 1,
            }
        }
        let fresh = items.len();
        let merge = earlier.len() >= MAX_SEALED;
        if merge {
            for (sealed_key, _) in &earlier {
                handle
                    .read(sealed_key, |entry| {
                        sealed::decode(entry.bytes).map(|(_, theirs)| {
                            // alloc: as `items`
                            items.extend(theirs.into_iter().map(|(key, bytes)| (key, bytes.to_vec())));
                        })
                    })
                    // alloc: error path
                    .map_err(|corrupt| format!("the sealed set {:?} of the set {set:?} does not read: {corrupt}", sealed_key.0))?
                    // alloc: error path
                    .ok_or_else(|| format!("the sealed set {:?} of the set {set:?} is not in the store", sealed_key.0))?
                    // alloc: error path
                    .map_err(|why| format!("the sealed set {:?} of the set {set:?}: {why}", sealed_key.0))?;
            }
        }
        if fresh == 0 {
            return Ok(None);
        }
        // By key; of two of one key the first is kept: what is held or a record, before an
        // earlier sealed set's (the same bytes: a key covers every input of its item).
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let with_merged = items.len();
        items.dedup_by(|later, first| later.0 == first.0);
        let merged = (with_merged - fresh).saturating_sub(with_merged - items.len()) as u64;
        // alloc: once a seal, a reference an item
        let refs: Vec<(Key, &[u8])> = items.iter().map(|(key, bytes)| (*key, bytes.as_slice())).collect();
        let bytes = sealed::encode(set, &refs)?;
        let sealed_key = sealed::key_of(&bytes);
        let header = sealed::Header::decode(&bytes).expect("the header just encoded");
        let first = handle
            .put(sealed_key, Kind::Query, &bytes)
            // alloc: error path
            .map_err(|mismatch| format!("the sealed set of the set {set:?} was refused: {mismatch:?}"))?;
        let mut sealed_sets = if merge { Vec::new() } else { earlier }; // alloc: the set's few sealed sets
        if !sealed_sets.iter().any(|it| it.0 == sealed_key) {
            sealed_sets.push((sealed_key, header)); // alloc: as above
        }
        // alloc: as above
        let mut named: Vec<Key> = sealed_sets.iter().map(|it| it.0).collect();
        #[cfg(test)]
        if SEALED_NOT_NAMED.with(|it| it.get()) {
            named.retain(|it| *it != sealed_key);
        }
        let (_, (root, root_bytes)) = listing_of(set, &[], &named);
        let second = handle
            .put(root, Kind::Set, &root_bytes)
            // alloc: error path
            .map_err(|mismatch| format!("the root of the set {set:?} was refused: {mismatch:?}"))?;
        let mut shard = shard_of();
        let members = shard.get_mut(set).expect("a set is never removed");
        // What was got or put while the seal ran is still a member, or still held.
        members.keys.retain(|key| keys.binary_search(key).is_err());
        if let Some(held) = members.held.as_mut() {
            for key in &held_keys {
                held.remove(key);
            }
        }
        members.listed = 0;
        members.leaves = Vec::new();
        members.base = None;
        members.sealed = sealed_sets;
        members.root = Some(root);
        members.quiet = 0;
        Ok(Some(Sealing {
            root,
            sealed: sealed_key,
            items: refs.len() as u64,
            bytes: bytes.len() as u64,
            merged,
            gone,
            durable: first.max(second),
        }))
    }
}

#[cfg(test)]
thread_local! {
    /// A planted bug for the file set's simulation (`tests::collect`): the pin written before
    /// a file is made durable names the next file's set alone, letting go of the durable
    /// file's before the next one is.
    pub(crate) static FILE_SET_EARLY_RELEASE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What [`FileSet::start`] found of the set the durable file names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileSetStart {
    /// No file is durable (none, or one the start does not use): the pin lets go of
    /// whatever an earlier run left under its name.
    NoFile,
    /// The run that made the file durable listed its set: pinned as it was.
    Listed { root: Key, keys: u64 },
    /// The store holds no listing of the file's set: a store of the layout before a file's
    /// set was pinned. Listed and pinned now; `missing` of its keys the store does not hold
    /// (a collection before this start forgot them, and a read of one fails naming it).
    Unlisted { root: Key, keys: u64, missing: u64 },
}

/// What [`FileSet::prepare`] says of the file about to be made durable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prepared {
    /// Its set is listed and pinned, durably, beside the set of the file durable now: the
    /// file may be made durable.
    Ready,
    /// The listing or the pin is written and not durable yet: flush the store and ask again.
    Flush,
}

/// The keys a file outside the store names (the daemon image's file texts), kept from the
/// collector across every change of that file and every crash. The keys are listed as a set
/// is (a [`Kind::Set`] listing, leaves shared between files that name the same runs of keys)
/// and one pin, `name`, names the listings' roots: the durable file's, and from
/// [`FileSet::prepare`] until [`FileSet::placed`] the next file's beside it. A sans-IO core:
/// it writes records and pins into the store it is given and owns no IO.
///
/// The invariant: the set of the file durable on disk is rooted at every point, in memory and
/// in the store's durable state: the next file's set is pinned, beside it, durably before a
/// byte of the next file is written ([`Prepared::Ready`]); the durable file's set is let go of
/// only once the next file is ([`FileSet::placed`], then [`FileSet::settle`]); and a start
/// lists and pins again what the file on disk names ([`FileSet::start`]). Tested in
/// `tests::collect` (a collection after every step, a crash at every step, the planted early
/// release [`FILE_SET_EARLY_RELEASE`]).
pub struct FileSet {
    name: &'static str,
    why: &'static str,
    /// The id the listings are of.
    set: ContentId,
    /// The root of the listing of what the durable file names, with its leaves (ascending).
    on_disk: Option<(Key, Vec<Key>)>,
    /// The same of the file being made durable, from [`FileSet::prepare`] on: none while it
    /// names what the durable file names.
    next: Option<(Key, Vec<Key>)>,
    /// The roots the pin names as this run last wrote it; none before its first write.
    pinned: Option<Vec<Key>>,
    /// What the listings and the pins written wait on to be durable.
    pending: Tickets,
}

impl FileSet {
    /// The set of the file durable at a start: `keys`, what it names, or none when no file is
    /// durable or the start does not use it. Its listing is put (a record the store has is not
    /// written again) and the pin `name` names its root alone: whatever an earlier run left
    /// pinned beside it (a file it was making when it stopped) is let go of. In memory at once,
    /// so no collection after this misses it, and durable with the store's next flush; a crash
    /// before then leaves the store's pin as it was, and the next start does this again.
    pub fn start(
        store: &mut Store,
        name: &'static str,
        why: &'static str,
        set: ContentId,
        keys: Option<Vec<Key>>,
        when: u64,
    ) -> Result<(FileSet, FileSetStart), Mismatch> {
        let mut it =
            FileSet { name, why, set, on_disk: None, next: None, pinned: None, pending: Tickets::default() };
        let Some(keys) = keys else {
            it.write(store, when)?;
            return Ok((it, FileSetStart::NoFile));
        };
        let (keys, (root, root_bytes), leaves) = it.listing(keys);
        let listed = store.has(&root);
        let missing = match listed {
            true => 0,
            false => keys.iter().filter(|key| !store.has(key)).count() as u64,
        };
        it.put(store, &leaves, (root, root_bytes.as_slice()))?;
        it.on_disk = Some((root, leaf_keys(&leaves)));
        it.write(store, when)?;
        let count = keys.len() as u64;
        Ok((it, match listed {
            true => FileSetStart::Listed { root, keys: count },
            false => FileSetStart::Unlisted { root, keys: count, missing },
        }))
    }

    /// Before the next file, which names `keys`, is made durable: its set is listed and the pin
    /// names its root beside the durable file's. [`Prepared::Ready`] once both are durable:
    /// only then may a byte of the file be written. Asked again for a file that names other
    /// keys (what it was to name changed before it was made), the set asked before is let go
    /// of; asked again for the same keys, nothing is written.
    pub fn prepare(&mut self, store: &mut Store, keys: Vec<Key>, when: u64) -> Result<Prepared, Mismatch> {
        let (_, (root, root_bytes), leaves) = self.listing(keys);
        self.put(store, &leaves, (root, root_bytes.as_slice()))?;
        self.next = match &self.on_disk {
            Some((disk, _)) if *disk == root => None,
            _ => Some((root, leaf_keys(&leaves))),
        };
        self.write(store, when)?;
        self.pending.retain_pending(|ticket| store.durable(ticket));
        Ok(match self.pending.is_empty() {
            true => Prepared::Ready,
            false => Prepared::Flush,
        })
    }

    /// The file last prepared is durable on disk: its set is the durable file's from now on,
    /// and the pin lets go of the set before at the next [`FileSet::settle`]. No IO: the
    /// caller may hold no store here.
    pub fn placed(&mut self) {
        if let Some(next) = self.next.take() {
            self.on_disk = Some(next);
        }
    }

    /// Writes the pin when what it is to name changed since its last write (a file placed):
    /// asked each turn the store is at hand. Durable with the store's next flush; until then
    /// the store's pin names one set more than it needs to, never one less.
    pub fn settle(&mut self, store: &mut Store, when: u64) -> Result<(), Mismatch> {
        self.write(store, when)
    }

    /// The root of the durable file's set, and of the next file's while one is being made.
    pub fn roots(&self) -> (Option<Key>, Option<Key>) {
        (self.on_disk.as_ref().map(|it| it.0), self.next.as_ref().map(|it| it.0))
    }

    /// `keys` ascending without repeats, and their listing: the root's key and bytes, and the
    /// leaves'.
    #[allow(clippy::type_complexity)]
    fn listing(&self, mut keys: Vec<Key>) -> (Vec<Key>, (Key, Vec<u8>), Vec<(Key, Vec<u8>)>) {
        keys.sort_unstable();
        keys.dedup();
        let (leaves, root) = listing_of(&self.set, &keys, &[]);
        (keys, root, leaves)
    }

    /// Puts the leaves and the root a listing is made of, but for the leaves the pin keeps
    /// already (the durable file's and the next file's): those are in the store, put by this
    /// run (their tickets among those waited on) or listed by the start.
    fn put(&mut self, store: &mut Store, leaves: &[(Key, Vec<u8>)], root: (Key, &[u8])) -> Result<(), Mismatch> {
        let known = |key: &Key| {
            [&self.on_disk, &self.next]
                .into_iter()
                .flatten()
                .any(|(_, leaves)| leaves.binary_search(key).is_ok())
        };
        for (key, bytes) in leaves.iter().map(|(key, bytes)| (*key, bytes.as_slice())).chain([root]) {
            if known(&key) {
                continue;
            }
            let ticket = store.put(key, Kind::Set, bytes)?;
            self.pending.add(ticket);
        }
        Ok(())
    }

    /// The pin as it is to be: the durable file's root and the next file's, written when that
    /// is not what it names now.
    fn write(&mut self, store: &mut Store, when: u64) -> Result<(), Mismatch> {
        // alloc: once a pin written, its one or two roots
        let mut roots: Vec<Key> = [&self.on_disk, &self.next].into_iter().flatten().map(|it| it.0).collect();
        #[cfg(test)]
        if let Some((next, _)) = &self.next
            && FILE_SET_EARLY_RELEASE.with(|it| it.get())
        {
            roots.retain(|it| it == next);
        }
        roots.sort_unstable();
        if self.pinned.as_ref() == Some(&roots) {
            return Ok(());
        }
        let ticket = store.pin(self.name, self.why, when, &roots)?;
        self.pending.add(ticket);
        self.pinned = Some(roots);
        Ok(())
    }
}

/// The keys of a listing's leaves, ascending: what [`FileSet`] searches.
fn leaf_keys(leaves: &[(Key, Vec<u8>)]) -> Vec<Key> {
    // alloc: once a listing kept, a key a leaf
    let mut keys: Vec<Key> = leaves.iter().map(|it| it.0).collect();
    keys.sort_unstable();
    keys
}
