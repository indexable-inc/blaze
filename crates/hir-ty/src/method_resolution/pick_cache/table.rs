//! A fixed-size map shared by every thread, from a key in a state (a revision) to a value made
//! once: the pick cache's and the steps cache's storage.
//!
//! A slot is a hint (a hash of its key, 0 for none) and a pointer to its entry, side by side in
//! 16 bytes, so a lookup loads the slot's line and then the entry's. An entry is never changed
//! once published but for its use mark; a slot is given another entry by a compare-and-swap of
//! its pointer, and the entry it held is retired to the table's limbo and freed once every task
//! that may have loaded it has ended (`blaze_reclaim`). Lookups take no lock and write nothing
//! but the use mark, once per sweep.
//!
//! The unsafe is the entry pointers: turning a published one back into a reference, and the box
//! round trip that hands an unlinked entry to the limbo. The safe version, an `Arc` per entry
//! or a lock per slot, costs a hit two read-modify-writes on a line every thread shares (or a
//! lock), where this costs two loads. `model.rs` checks it under `blaze_check`.

use std::ptr;

#[cfg(blaze_check)]
use blaze_check::sync::{
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    lot::Mutex,
};
use blaze_reclaim::{Guard, Limbo};
#[cfg(not(blaze_check))]
use parking_lot::Mutex;
#[cfg(not(blaze_check))]
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

#[cfg(all(test, blaze_check))]
mod model;

/// A key of a [`Table`].
pub(super) trait Key: Copy + Eq + Send + Sync + 'static {
    /// Never [`EMPTY`], and the same for equal keys.
    fn hint(&self) -> u64;
}

const EMPTY: u64 = 0;

/// How many slots from the one a key hashes to it may be kept in.
const PROBE: usize = 16;

#[repr(C, align(16))]
struct Slot<K, S, V> {
    hint: AtomicU64,
    entry: AtomicPtr<Entry<K, S, V>>,
}

const _: () = assert!(size_of::<Slot<u64, u64, u64>>() == 16);

struct Entry<K, S, V> {
    key: K,
    state: S,
    /// Set by a use, cleared by a sweep passing over it: of a full window, the first entry not
    /// used since the last sweep is given away.
    referenced: AtomicBool,
    value: V,
}

impl<K, S, V> Entry<K, S, V> {
    #[inline]
    fn touch(&self) {
        // A load first: a hit writes nothing but once per sweep.
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
    }
}

pub(super) struct Table<K, S, V> {
    slots: Box<[Slot<K, S, V>]>,
    /// Entries replaced, until no task can still be reading them. Locked only to replace.
    limbo: Mutex<Limbo>,
}

/// A slot and the entry it was seen to hold.
type Held<K, S, V> = (usize, *mut Entry<K, S, V>);

impl<K, S, V> Table<K, S, V>
where
    K: Key,
    S: Copy + Eq + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub(super) fn new(bits: u32) -> Table<K, S, V> {
        assert!((1..32).contains(&bits), "a table of 2^{bits} slots");
        let slots = (0..1usize << bits)
            .map(|_| Slot { hint: AtomicU64::new(EMPTY), entry: AtomicPtr::new(ptr::null_mut()) });
        Table {
            // alloc: setup, once per table and process.
            slots: slots.collect(),
            limbo: Mutex::new(Limbo::new()),
        }
    }

    /// The value of `key` in `state`, good while `guard` is.
    #[inline]
    pub(super) fn get<'g>(&'g self, _guard: &'g Guard, key: &K, state: S) -> Option<&'g V> {
        let hint = key.hint();
        for at in window(hint, self.slots.len()) {
            let slot = &self.slots[at];
            if slot.hint.load(Ordering::Acquire) != hint {
                continue;
            }
            let Some(entry) = self.entry(slot.entry.load(Ordering::Acquire)) else { continue };
            if entry.key == *key && entry.state == state {
                entry.touch();
                return Some(&entry.value);
            }
        }
        None
    }

    /// Stores `value` as the value of `key` in `state` and returns it; or, when another thread
    /// stored one first, drops `value` and returns that one. Good while `guard` is.
    #[cold]
    pub(super) fn insert<'g>(&'g self, _guard: &'g Guard, key: K, state: S, value: V) -> &'g V {
        let hint = key.hint();
        // The box is what the limbo frees after a grace.
        // alloc: a miss, once per key and state, after a probe made the value.
        let made = Box::into_raw(Box::new(Entry {
            key,
            state,
            referenced: AtomicBool::new(true),
            value,
        }));
        loop {
            let (at, held) = match self.victim(hint, &key, state) {
                Ok(won) => {
                    // SAFETY: `made` was never published, so this is its only owner.
                    drop(unsafe { Box::from_raw(made) });
                    return won;
                }
                Err(slot) => slot,
            };
            let slot = &self.slots[at];
            if slot
                .entry
                .compare_exchange(held, made, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // Another thread changed the slot first: look again.
                continue;
            }
            slot.hint.store(hint, Ordering::Release);
            if !held.is_null() {
                // SAFETY: every entry comes from `Box::into_raw` here, and the swap above
                // unlinked `held`: one swap alone can, so this is the one place that takes it
                // back, and the limbo frees it only after every task that may have loaded it has
                // ended.
                self.retire(unsafe { Box::from_raw(held) });
            }
            // SAFETY: published above; freed only by the limbo, after a grace that starts once
            // it is unlinked, which is after `guard`, entered before this call, was.
            return unsafe { &(*made).value };
        }
    }

    /// The entry `held` points to, if any. Called only on a pointer loaded under the guard of a
    /// `get` or `insert` call, and good while that guard is.
    #[inline]
    fn entry(&self, held: *mut Entry<K, S, V>) -> Option<&Entry<K, S, V>> {
        // SAFETY: an entry is freed only by the limbo, after a grace started once it was
        // unlinked from its slot; the caller's guard was entered before `held` was loaded, so
        // that grace has not passed while the guard lives, and `get`/`insert` tie what they
        // return to it.
        unsafe { held.as_ref() }
    }

    /// The slot `key` (hashing to `hint`) is given, and what it holds: one holding `key` in an
    /// older state, else an empty one, else the first not used since a sweep last passed over
    /// it, clearing the mark of each one passed over. Or the value of `key` in `state`, if a
    /// thread stored it first.
    #[cold]
    fn victim(&self, hint: u64, key: &K, state: S) -> Result<&V, Held<K, S, V>> {
        let len = self.slots.len();
        let (mut stale, mut empty, mut first) = (None, None, None);
        for at in window(hint, len) {
            let held = self.slots[at].entry.load(Ordering::Acquire);
            let Some(entry) = self.entry(held) else {
                empty.get_or_insert(at);
                continue;
            };
            if entry.key == *key {
                if entry.state == state {
                    entry.touch();
                    return Ok(&entry.value);
                }
                stale.get_or_insert((at, held));
            }
            first.get_or_insert((at, held));
        }
        if let Some(stale) = stale {
            return Err(stale);
        }
        if let Some(at) = empty {
            return Err((at, ptr::null_mut()));
        }
        for _ in 0..2 {
            for at in window(hint, len) {
                let held = self.slots[at].entry.load(Ordering::Acquire);
                let Some(entry) = self.entry(held) else {
                    return Err((at, ptr::null_mut()));
                };
                if !entry.referenced.load(Ordering::Relaxed) {
                    return Err((at, held));
                }
                entry.referenced.store(false, Ordering::Relaxed);
            }
        }
        // Every slot was used again while being swept: the first one goes.
        Err(first.expect("a window has a slot"))
    }

    fn retire(&self, entry: Box<Entry<K, S, V>>) {
        let mut limbo = self.limbo.lock();
        limbo.retire(entry);
        // alloc: `Limbo::collect` frees; it is not an iterator's.
        limbo.collect();
    }
}

impl<K, S, V> Drop for Table<K, S, V> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            let held = std::mem::replace(slot.entry.get_mut(), ptr::null_mut());
            if !held.is_null() {
                // SAFETY: `&mut self`: no reader is left, and each entry is in one slot.
                drop(unsafe { Box::from_raw(held) });
            }
        }
    }
}

/// The slots a key hashing to `hash` may be in, out of `len`, the one it hashes to first.
fn window(hash: u64, len: usize) -> impl Iterator<Item = usize> {
    let mask = len - 1;
    let start = (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - len.trailing_zeros())) as usize;
    (0..PROBE.min(len)).map(move |step| (start + step) & mask)
}
