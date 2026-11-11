//! Which slot a shape takes among those of its key, for the goal store's flush and the item
//! store's (`crate::body_store`): one key (a goal's, or a body's step's base key) keeps a few
//! shapes at once, each in a slot of its own, and a flush places each shape it writes in the
//! first slot that holds it or is free.
//!
//! One flush can write several shapes of one key: two bodies alike enough to share an identity
//! whose steps read different facts, a goal evaluated again after a write. So a flush places
//! the entries of one key one after another, in an order that is of their bytes alone
//! ([`groups`]), against what the flush itself placed before ([`Claimed`]): the store's `get`
//! need not show a put the driver has not made yet (blaze-cargo's buffers every put until the
//! flush has ended), and which slot a shape takes must not depend on which thread wrote first.

use std::{cmp::Ordering, ops::Range};

use blaze_ids::ContentId;
use hir_def::deps::ImageStore;

/// The shapes one flush found or put in the slots of one key, by slot.
pub(crate) struct Claimed<'a, const SLOTS: usize> {
    shapes: [Option<&'a [u8]>; SLOTS],
    /// The planted bug of the flush's control: nothing placed is remembered, so the second
    /// shape of a key is placed against the store alone.
    #[cfg(test)]
    forgetful: bool,
}

/// Where a shape was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) n: u32,
    /// Whether this placing put the shape, or the slot held it already.
    pub(crate) written: bool,
}

/// Why a shape could not be placed: what the flush was `doing` and the store's reason.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) doing: &'static str,
    pub(crate) why: String,
}

#[cfg(test)]
thread_local! {
    /// Set by a control on its own thread: every [`Claimed`] made on it forgets what it placed.
    pub(crate) static FORGETFUL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl<'a, const SLOTS: usize> Claimed<'a, SLOTS> {
    pub(crate) fn new() -> Claimed<'a, SLOTS> {
        Claimed {
            shapes: [None; SLOTS],
            #[cfg(test)]
            forgetful: FORGETFUL.get(),
        }
    }

    #[cfg(test)]
    fn forgets(&self) -> bool {
        self.forgetful
    }

    #[cfg(not(test))]
    fn forgets(&self) -> bool {
        false
    }

    fn remember(&mut self, n: usize, shape: &'a [u8]) {
        if !self.forgets() {
            self.shapes[n] = Some(shape);
        }
    }

    /// Places `shape` in the first slot that holds it or is free, `at(n)` the store key of slot
    /// `n`: `None` when every slot holds another shape.
    ///
    /// A slot this flush placed a shape in is known from here; any other is read from the
    /// store, and a free one is put. A put the store refuses is read again: another writer's
    /// shape kept in between takes the slot, and this one goes on to the next.
    pub(crate) fn claim(
        &mut self,
        // dyn: the driver's store, as the stores that flush hold it
        images: &dyn ImageStore,
        shape: &'a [u8],
        at: impl Fn(u32) -> ContentId,
    ) -> Result<Option<Slot>, Refused> {
        for n in 0..SLOTS {
            let slot = u32::try_from(n).expect("a few slots");
            match self.shapes[n] {
                Some(placed) if placed == shape => return Ok(Some(Slot { n: slot, written: false })),
                Some(_) => continue,
                None => {}
            }
            let key = at(slot);
            let read = || {
                images.get(&key).map_err(|why| Refused { doing: "reading the shape of", why })
            };
            match read()? {
                Some(kept) if kept.bytes() == shape => {
                    self.remember(n, shape);
                    return Ok(Some(Slot { n: slot, written: false }));
                }
                Some(_) => continue,
                None => {}
            }
            // alloc: the shape's bytes, once per shape written
            match images.put(key, shape.to_vec()) {
                Ok(()) => {
                    self.remember(n, shape);
                    return Ok(Some(Slot { n: slot, written: true }));
                }
                Err(why) => match read()? {
                    Some(kept) if kept.bytes() == shape => {
                        self.remember(n, shape);
                        return Ok(Some(Slot { n: slot, written: false }));
                    }
                    Some(_) => continue,
                    None => return Err(Refused { doing: "keeping the shape of", why }),
                },
            }
        }
        Ok(None)
    }
}

/// The entries `0..count` of a flush in the order it writes them (`order`, by `cmp`: of the
/// entries' bytes alone, so the same entries are placed alike whichever thread offered them),
/// and each run of entries of one key (`same`) as a range of that order: the runs are written
/// at once, the entries of a run one after another.
pub(crate) fn groups(
    count: usize,
    cmp: impl Fn(usize, usize) -> Ordering,
    same: impl Fn(usize, usize) -> bool,
) -> (Vec<usize>, Vec<Range<usize>>) {
    // alloc: flush, once per build: one index per entry
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| cmp(a, b));
    // alloc: flush, once per build: one range per key
    let mut runs = Vec::new();
    let mut start = 0;
    for at in 1..=order.len() {
        if at == order.len() || !same(order[at - 1], order[at]) {
            runs.push(start..at);
            start = at;
        }
    }
    (order, runs)
}
