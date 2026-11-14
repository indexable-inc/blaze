//! An arena's table: an insert-only hash trie, every node and entry in the arena.
//!
//! A node is 16 slots (128 bytes); a slot is null, an entry, or a child node (the pointer's
//! low bit set). A key's path is its 64-bit hash, four bits per level from the low end, so a
//! table of `n` entries is about `log16 n` levels deep. Two entries whose hashes are equal in
//! all 64 bits chain through their heads' `chain`.
//!
//! Nothing is ever removed or moved within an arena's life, so a reader needs no lock and no
//! reclamation: it follows acquire loads, and every pointer it reads stays valid as long as the
//! arena. A writer publishes an entry fully written, with one release compare-and-swap; a slot
//! that holds an entry and must hold two becomes a child node holding the old entry, by one
//! compare-and-swap of the slot (a lost race leaves the losing node unreachable in the arena,
//! 128 bytes, and retries). The cost: a lookup is at most 16 dependent loads and in practice
//! `log16 n + 1`; an insert is the same walk plus one compare-and-swap, plus a node per level
//! two keys share.

use std::{
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use blaze_mem::ArenaLocal;

use crate::{Fact, Kind};

const FANOUT: usize = 16;
const BITS: u32 = 4;
const NODE: usize = 1;

/// The fixed part of every entry, at its start (`#[repr(C)]`), so a slot can point at any
/// entry type and a reader checks the kind before it casts.
#[repr(C)]
pub(crate) struct Head {
    pub(crate) hash: u64,
    pub(crate) kind: &'static Kind,
    /// The next entry of the same full hash.
    chain: AtomicPtr<Head>,
    /// The entry's read list ([`crate::Reads::Kept`]), in the same arena; empty otherwise.
    reads: *const Fact,
    reads_len: u32,
    /// An interned value's or input's index in its arena's column; 0 for a query's entry.
    pub(crate) local: u32,
}

impl Head {
    pub(crate) fn new(hash: u64, kind: &'static Kind, reads: &[Fact], local: u32) -> Head {
        Head {
            hash,
            kind,
            chain: AtomicPtr::new(ptr::null_mut()),
            reads: reads.as_ptr(),
            reads_len: u32::try_from(reads.len()).expect("a read list of under 2^32 facts"),
            local,
        }
    }

    /// The entry's read list.
    pub(crate) fn reads(&self) -> &[Fact] {
        if self.reads_len == 0 {
            return &[];
        }
        // SAFETY: `reads` was a slice of `reads_len` facts in the entry's own arena, which
        // lives as long as the entry.
        unsafe { std::slice::from_raw_parts(self.reads, self.reads_len as usize) }
    }
}

/// A query's, an interned struct's or an input's entry: the head, then its key and value.
#[repr(C)]
pub(crate) struct Entry<K, V> {
    pub(crate) head: Head,
    pub(crate) key: K,
    pub(crate) value: V,
}

impl<K, V> Entry<K, V> {
    /// The entry `head` starts.
    ///
    /// # Safety
    ///
    /// `head` is the head of an `Entry<K, V>` (its kind says so).
    pub(crate) unsafe fn of(head: &Head) -> &Entry<K, V> {
        // SAFETY: the caller's; `Entry` is `#[repr(C)]` with the head first.
        unsafe { &*ptr::from_ref(head).cast::<Entry<K, V>>() }
    }
}

struct Node {
    slots: [AtomicPtr<()>; FANOUT],
}

impl Node {
    const EMPTY: Node = Node { slots: [const { AtomicPtr::new(ptr::null_mut()) }; FANOUT] };
}

/// An insert-only table of entries keyed by a 64-bit hash and an equality.
pub(crate) struct Trie {
    root: AtomicPtr<Node>,
}

// SAFETY: every shared field is atomic, and the entries it reaches are immutable once
// published; the values they hold are `Send + Sync` (each typed insert says so).
unsafe impl Send for Trie {}
// SAFETY: as for `Send`.
unsafe impl Sync for Trie {}

fn index(hash: u64, depth: u32) -> usize {
    ((hash >> (depth * BITS)) & (FANOUT as u64 - 1)) as usize
}

fn is_node(it: *mut ()) -> bool {
    it as usize & NODE != 0
}

/// The node a slot's tagged pointer is.
///
/// # Safety
///
/// `it` is tagged as a node and points at a node in the table's arena.
unsafe fn node<'a>(it: *mut ()) -> &'a Node {
    // SAFETY: the caller's.
    unsafe { &*((it as usize & !NODE) as *const Node) }
}

impl Trie {
    pub(crate) const fn new() -> Trie {
        Trie { root: AtomicPtr::new(ptr::null_mut()) }
    }

    /// The entry of `hash` that `same` accepts.
    pub(crate) fn get(&self, hash: u64, same: impl Fn(&Head) -> bool) -> Option<&Head> {
        let root = self.root.load(Ordering::Acquire);
        if root.is_null() {
            return None;
        }
        // SAFETY: a published node lives as long as the arena, which outlives `self`'s borrow.
        let mut slots = unsafe { &(*root).slots };
        let mut depth = 0;
        loop {
            let it = slots[index(hash, depth)].load(Ordering::Acquire);
            if it.is_null() {
                return None;
            }
            if is_node(it) {
                // SAFETY: tagged as a node, published by release.
                slots = unsafe { &node(it).slots };
                depth += 1;
                continue;
            }
            // SAFETY: an untagged non-null slot is a published entry's head.
            let mut at = unsafe { &*it.cast::<Head>() };
            if at.hash != hash {
                return None;
            }
            loop {
                if same(at) {
                    return Some(at);
                }
                let next = at.chain.load(Ordering::Acquire);
                if next.is_null() {
                    return None;
                }
                // SAFETY: a chained head is published by release.
                at = unsafe { &*next };
            }
        }
    }

    /// Publishes `new` (fully written, in `local`'s arena, which is the table's), or gives the
    /// entry already there that `same` accepts.
    pub(crate) fn insert<'t>(
        &'t self,
        local: &ArenaLocal<'_>,
        new: &'t Head,
        same: impl Fn(&Head) -> bool,
    ) -> Result<(), &'t Head> {
        let hash = new.hash;
        let new_ptr = ptr::from_ref(new).cast_mut().cast::<()>();
        let mut slots = &self.root_in(local).slots;
        let mut depth = 0;
        loop {
            let slot = &slots[index(hash, depth)];
            let it = slot.load(Ordering::Acquire);
            if it.is_null() {
                match slot.compare_exchange(
                    ptr::null_mut(),
                    new_ptr,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(()),
                    Err(_) => continue,
                }
            }
            if is_node(it) {
                // SAFETY: tagged as a node, published by release.
                slots = unsafe { &node(it).slots };
                depth += 1;
                continue;
            }
            // SAFETY: an untagged non-null slot is a published entry's head.
            let there = unsafe { &*it.cast::<Head>() };
            if there.hash == hash {
                return Self::chain(there, new, same);
            }
            // Two hashes that agree on every nibble below `depth` and differ: they differ at
            // `depth` or deeper, so `depth < 16` here and the child is at most level 15.
            let child = local.alloc(Node::EMPTY);
            child.slots[index(there.hash, depth + 1)].store(it, Ordering::Relaxed);
            let tagged = (ptr::from_ref::<Node>(child) as usize | NODE) as *mut ();
            match slot.compare_exchange(it, tagged, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    slots = &child.slots;
                    depth += 1;
                }
                // Lost to another writer: the child is unreachable; walk again.
                Err(_) => continue,
            }
        }
    }

    /// `new` appended to the chain of entries of its full hash that starts at `first`, or the
    /// one there that `same` accepts.
    fn chain<'t>(
        first: &'t Head,
        new: &'t Head,
        same: impl Fn(&Head) -> bool,
    ) -> Result<(), &'t Head> {
        let new_ptr = ptr::from_ref(new).cast_mut();
        let mut at = first;
        loop {
            if same(at) {
                return Err(at);
            }
            let next = at.chain.load(Ordering::Acquire);
            if next.is_null() {
                match at.chain.compare_exchange(
                    ptr::null_mut(),
                    new_ptr,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(()),
                    // Another entry was chained here first: look at it next.
                    Err(_) => continue,
                }
            }
            // SAFETY: a chained head is published by release.
            at = unsafe { &*next };
        }
    }

    fn root_in(&self, local: &ArenaLocal<'_>) -> &Node {
        let root = self.root.load(Ordering::Acquire);
        if !root.is_null() {
            // SAFETY: published by release, in the arena.
            return unsafe { &*root };
        }
        let made = ptr::from_mut(local.alloc(Node::EMPTY));
        match self.root.compare_exchange(
            ptr::null_mut(),
            made,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: just published, in the arena.
            Ok(_) => unsafe { &*made },
            // SAFETY: the winner's, published by release.
            Err(theirs) => unsafe { &*theirs },
        }
    }
}
