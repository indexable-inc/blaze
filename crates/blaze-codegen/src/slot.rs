//! A record written once and read with no lock: what a walk's function, a program's dyn facts
//! and a named target are kept in (docs/v2/plan/design/scheduler.md, rule 4 of its model: no
//! shared mutable state in a node). A slot is about one key, so many threads write and read
//! many slots at once and none waits for another: a read is one acquire load, and two writers
//! of one slot meet only for the move of the value, never while either computes it.
//!
//! A second write never replaces the first. Of a value that compares, an equal one is the one
//! kept ([`Slot::put`]); another is [`Refused`], which carries both for the caller's error to
//! name the key by (rule 21: two values under one key mean the key misses an input).

#![forbid(unsafe_code)]

/// A value written once ([`Slot::set`], [`Slot::put`]) and read with no lock ([`Slot::get`]).
#[expect(
    clippy::disallowed_types,
    reason = "a slot set once, read with an acquire load: one key's record, not global state"
)]
pub struct Slot<T> {
    value: std::sync::OnceLock<T>,
}

/// A write a [`Slot`] refused: it holds `kept`, and `given` came after.
#[derive(Debug)]
pub struct Refused<'s, T> {
    pub kept: &'s T,
    pub given: T,
}

#[expect(
    clippy::disallowed_types,
    reason = "a slot set once, read with an acquire load: one key's record, not global state"
)]
impl<T> Slot<T> {
    /// A slot nothing was written to.
    pub const fn new() -> Slot<T> {
        Slot { value: std::sync::OnceLock::new() }
    }

    /// What was written, with no lock; `None` before the write.
    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    /// Writes `value` where nothing was written, and gives it as kept. A slot that holds a
    /// value refuses, whatever `value` is: for a record only one writer makes (a function's
    /// describe, by the one visit of it), where a second write is the caller's bug to name.
    ///
    /// # Errors
    ///
    /// [`Refused`], with the value kept and the one given.
    pub fn set(&self, value: T) -> Result<&T, Refused<'_, T>> {
        let given = self.value.set(value).err();
        // Set by this call or by one before it: `OnceLock::set` returns once it holds a value.
        let kept = self.value.get().expect("a slot holds a value once a write to it returned");
        match given {
            None => Ok(kept),
            Some(given) => Err(Refused { kept, given }),
        }
    }

    /// The value written, if one was.
    pub fn into_inner(self) -> Option<T> {
        self.value.into_inner()
    }
}

impl<T: PartialEq> Slot<T> {
    /// Writes `value` where nothing was written, and gives the value kept: `value`, or an
    /// equal one written before (two writers that computed one record of one key).
    ///
    /// # Errors
    ///
    /// [`Refused`] where the slot holds another value: never written over.
    pub fn put(&self, value: T) -> Result<&T, Refused<'_, T>> {
        match self.set(value) {
            Err(refused) if *refused.kept == refused.given => Ok(refused.kept),
            written => written,
        }
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Slot<T> {
        Slot::new()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.get() {
            Some(value) => f.debug_tuple("Slot").field(value).finish(),
            None => f.write_str("Slot(unset)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;

    const THREADS: usize = 16;
    const KEYS: usize = 64;

    /// What `writer` writes under `key`: one value a key when `agree`, else its own.
    fn value(key: usize, writer: usize, agree: bool) -> u64 {
        match agree {
            true => key as u64 * 31 + 7,
            false => (key * THREADS + writer) as u64,
        }
    }

    /// The oracle. Every thread writes every key of `cells` through `write` (`None` for a
    /// write refused) and reads it back through `read`, all at once; then one more writer
    /// writes another value under every key. How many writes were refused, and whether a key
    /// held one value for good: each read after a write, by any thread, gave what that write
    /// said is kept, and the late writer changed nothing.
    fn race<C: Sync>(
        cells: &[C],
        agree: bool,
        write: impl Fn(&C, u64) -> Option<u64> + Sync,
        read: impl Fn(&C) -> Option<u64> + Sync,
    ) -> (usize, bool) {
        let refused = AtomicUsize::new(0);
        let changed = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for writer in 0..THREADS {
                let (refused, changed, write, read) = (&refused, &changed, &write, &read);
                scope.spawn(move || {
                    // Each writer from another key, so the first write of a key is not always
                    // the same thread's.
                    for step in 0..KEYS {
                        let key = (step + writer * 5) % KEYS;
                        let kept = match write(&cells[key], value(key, writer, agree)) {
                            Some(kept) => kept,
                            None => {
                                refused.fetch_add(1, Ordering::Relaxed);
                                read(&cells[key]).expect("a key that refused a write holds one")
                            }
                        };
                        if read(&cells[key]) != Some(kept) {
                            changed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        for cell in cells {
            let before = read(cell).expect("every key was written");
            if write(cell, before ^ u64::MAX).is_some() || read(cell) != Some(before) {
                changed.fetch_add(1, Ordering::Relaxed);
            }
        }
        (refused.into_inner(), changed.into_inner() == 0)
    }

    fn slots() -> Vec<Slot<u64>> {
        (0..KEYS).map(|_| Slot::new()).collect()
    }

    fn put(slot: &Slot<u64>, value: u64) -> Option<u64> {
        slot.put(value).ok().copied()
    }

    fn get(slot: &Slot<u64>) -> Option<u64> {
        slot.get().copied()
    }

    /// The invariant: a slot holds one value for good. Of many threads that write one key at
    /// once, each read by any of them gives the first value written; an equal write is that
    /// value ([`Slot::put`]), and another is refused with both values, the kept one staying.
    #[test]
    fn a_slot_keeps_its_first_value_and_refuses_another() {
        // Writers that agree: no refusal, and every key holds the value they all computed.
        let agreed = slots();
        assert_eq!(race(&agreed, true, put, get), (0, true));
        for (key, slot) in agreed.iter().enumerate() {
            assert_eq!(slot.get(), Some(&value(key, 0, true)), "key {key}");
        }

        // Writers that disagree: one write a key is kept, every other refused, none lost.
        let fought = slots();
        assert_eq!(race(&fought, false, put, get), (KEYS * (THREADS - 1), true));
        for (key, slot) in fought.iter().enumerate() {
            let kept = *slot.get().expect("every key was written");
            assert!(
                (0..THREADS).any(|writer| kept == value(key, writer, false)),
                "key {key} holds {kept}, which no writer wrote"
            );
        }

        // The refusal carries both values, for an error to name.
        let slot = Slot::new();
        assert_eq!(slot.put(1).ok(), Some(&1));
        let refused = slot.put(2).expect_err("another value under the key");
        assert_eq!((*refused.kept, refused.given), (1, 2));
        // `set` is for one writer: an equal second write is refused too.
        let refused = slot.set(1).expect_err("a second write");
        assert_eq!((*refused.kept, refused.given), (1, 1));
        assert_eq!(slot.into_inner(), Some(1));
    }

    /// The planted bug: a cell whose last writer wins. The oracle's late writer changes every
    /// key, whatever the threads before it did.
    #[test]
    fn the_oracle_catches_a_last_writer_that_wins() {
        const UNSET: u64 = u64::MAX;
        let cells: Vec<AtomicU64> = (0..KEYS).map(|_| AtomicU64::new(UNSET)).collect();
        let write = |cell: &AtomicU64, value: u64| {
            cell.store(value, Ordering::Release);
            Some(value)
        };
        let read = |cell: &AtomicU64| Some(cell.load(Ordering::Acquire)).filter(|it| *it != UNSET);
        let (refused, held) = race(&cells, false, write, read);
        assert_eq!((refused, held), (0, false), "a key written over went unseen");
    }
}
