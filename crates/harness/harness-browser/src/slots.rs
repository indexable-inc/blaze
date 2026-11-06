//! Which ring slot the host may write, sans IO: a synchronous state machine the paint and latch drivers in host.rs
//! feed, owning no surface, port or clock, so a deterministic simulation test drives it (tests below).
//!
//! The rule. A slot is writable when the app can neither be sampling it nor come to sample it: it is not the slot the
//! app last latched, and no frame naming it is outstanding (sent and not yet superseded by a later latch). The app
//! adopts frames in the order they were sent (Mach is FIFO, and it keeps only its newest pending frame), so its latch
//! of the frame with seq `s` means every frame sent before `s` is dead: it will never adopt one. Those slots, and the
//! slot it held before, come free; a frame sent after `s` stays outstanding, since the app may hold it pending.
//!
//! Each slot has at most one outstanding frame (a slot is claimed only when none is), so the app's latch, which names
//! a generation and a slot, names exactly one frame.
//!
//! When no slot is writable the paint is dropped and marked dirty; the latch that frees a slot asks for a repaint. A
//! frame the app's full queue refused frees its slot at once and asks for a repaint later (nothing else would ask:
//! the app may have no frame of ours to latch).

pub const SLOTS: usize = 3;

/// What the driver does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Nothing,
    /// Ask the browser to paint again now (`invalidate`): a dropped paint can go.
    Repaint,
    /// Ask again after a short while: the app's queue was full.
    RepaintLater,
}

/// A planted bug for the DST's controls (tests only): the oracle must catch each.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Plant {
    #[default]
    None,
    /// The old rule: write any slot that is neither latched nor the newest sent, outstanding or not.
    NeitherLatchedNorNewest,
    /// A latch frees every outstanding frame, the ones sent after it too.
    LatchFreesAll,
    /// A latch that frees a slot never asks for the dropped paint.
    NoRepaint,
}

/// One ring generation's slots.
#[derive(Debug)]
pub struct Slots {
    generation: u32,
    /// The slot the app last latched.
    latched: Option<u8>,
    /// Per slot, the seq of the frame naming it that the app may still adopt.
    out: [Option<u64>; SLOTS],
    /// The slot holding the newest picture written (a popup is composited over it).
    newest: Option<u8>,
    /// A paint was dropped for want of a slot.
    dirty: bool,
    #[cfg(test)]
    plant: Plant,
}

impl Slots {
    #[must_use]
    pub fn new(generation: u32) -> Self {
        Self {
            generation,
            latched: None,
            out: [None; SLOTS],
            newest: None,
            dirty: false,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    fn planted(generation: u32, plant: Plant) -> Self {
        Self { plant, ..Self::new(generation) }
    }

    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The slot holding the newest picture, if one was written.
    #[must_use]
    pub fn newest(&self) -> Option<u8> {
        self.newest
    }

    fn writable(&self, s: u8) -> bool {
        #[cfg(test)]
        if self.plant == Plant::NeitherLatchedNorNewest {
            return Some(s) != self.latched && Some(s) != self.newest;
        }
        // Not the newest either: a popup reads the newest while it writes the claimed slot.
        Some(s) != self.latched && Some(s) != self.newest && self.out[usize::from(s)].is_none()
    }

    /// A slot to write now, or `None`: the paint is dropped, and a later latch asks for it again.
    pub fn claim(&mut self) -> Option<u8> {
        let s = (0..SLOTS as u8).find(|s| self.writable(*s));
        if s.is_none() {
            self.dirty = true;
        }
        s
    }

    /// Slot `slot` (from [`Self::claim`]) now holds the newest picture, frame `seq`; `delivered` is false when the
    /// app's queue was full and the frame was not sent.
    pub fn sent(&mut self, slot: u8, seq: u64, delivered: bool) -> After {
        self.newest = Some(slot);
        if delivered {
            self.out[usize::from(slot)] = Some(seq);
            // This picture carries every change a dropped paint would have.
            self.dirty = false;
            After::Nothing
        } else {
            self.dirty = true;
            After::RepaintLater
        }
    }

    /// The app latched slot `slot` of generation `generation`.
    ///
    /// # Errors
    /// A slot outside the ring, or one no outstanding frame names (the app latched what it was never sent), naming
    /// the slot and generation.
    pub fn latched(&mut self, generation: u32, slot: u8) -> Result<After, String> {
        // A latch of an older ring is about surfaces already dropped here.
        if generation != self.generation {
            return Ok(After::Nothing);
        }
        if usize::from(slot) >= SLOTS {
            return Err(format!("the app latched slot {slot} of ring generation {generation}; a ring has {SLOTS}"));
        }
        let Some(seq) = self.out[usize::from(slot)] else {
            return Err(format!("the app latched slot {slot} of ring generation {generation}, which no outstanding frame names"));
        };
        for o in &mut self.out {
            let dead = o.is_some_and(|s| s < seq);
            #[cfg(test)]
            let dead = dead || (self.plant == Plant::LatchFreesAll && o.is_some());
            if dead {
                *o = None;
            }
        }
        self.out[usize::from(slot)] = None;
        self.latched = Some(slot);
        #[cfg(test)]
        if self.plant == Plant::NoRepaint {
            return Ok(After::Nothing);
        }
        if self.dirty && (0..SLOTS as u8).any(|s| self.writable(s)) {
            self.dirty = false;
            return Ok(After::Repaint);
        }
        Ok(After::Nothing)
    }
}

#[cfg(test)]
mod tests;
