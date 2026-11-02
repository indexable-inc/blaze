//! What a test process executed: one byte per jump-table slot of its binary, set on the
//! slot's first call (docs/v2/plan/lanes/fastcompile/select.md, "The record").
//!
//! The fork server maps a region of [`HEADER`] plus `slots` bytes shared (`MAP_SHARED`)
//! before it forks a test's child, with [`header`] written at its start. The child's runtime
//! arms every slot so its next call logs (`__blaze_exec_arm`, blaze-live's, over the jump table), first
//! marking what the server itself ran since it started, and says so by writing `armed`. A
//! slot's first call stores a non-zero byte at `HEADER + slot`, from any thread and from any
//! process the test forks, which shares the region. The server reads the region once the
//! child has ended, whether it exited or was killed, and sends the slots set
//! ([`Record::of_hits`]); [`Record::of_region`] is that reading, the reference the server's
//! own scan is checked against.
//!
//! A record is whole only when the runtime armed every slot and the child exited: anything
//! else is [`Record::Incomplete`], which no pass is ever kept by as executed.

pub use blaze_usercode::test::wire::{HEADER, MAGIC, header};

/// What a runtime that armed every slot of `region` writes. `false` when the region's header
/// is not one.
pub fn mark_armed(region: &mut [u8]) -> bool {
    let Some(head) = region.get_mut(..HEADER) else { return false };
    if head[..8] != MAGIC {
        return false;
    }
    let slots = [head[8], head[9], head[10], head[11]];
    head[12..16].copy_from_slice(&slots);
    true
}

/// A test process's record of what it executed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Record {
    /// Every slot was armed and the process ended as it should: `hits` are the slots called,
    /// strictly increasing, each below `slots`.
    Complete { slots: u32, hits: Vec<u32> },
    /// Not whole, and why: the binary keeps no record, the runtime did not arm, the process
    /// was killed or crashed, the record was cut.
    Incomplete { why: String },
}

impl Record {
    pub fn incomplete(why: impl Into<String>) -> Record {
        Record::Incomplete { why: why.into() }
    }

    /// The record a server sent: its hits must be strictly increasing and below `slots`,
    /// or the record is not one.
    pub fn of_hits(slots: u32, hits: Vec<u32>) -> Record {
        let mut last: Option<u32> = None;
        for &hit in &hits {
            if hit >= slots {
                // alloc: error path, the reason the record keeps
                return Record::incomplete(format!("its execution record names slot {hit} of a binary with {slots}"));
            }
            if last.is_some_and(|it| it >= hit) {
                // alloc: error path, the reason the record keeps
                return Record::incomplete(format!("its execution record names slot {hit} out of order"));
            }
            last = Some(hit);
        }
        Record::Complete { slots, hits }
    }

    /// `region` as a server reads it once the child has ended; `ended` is `Err` with why when
    /// it did not exit (a signal, a kill, a timeout).
    pub fn of_region(region: &[u8], ended: Result<(), &str>) -> Record {
        if let Err(why) = ended {
            // alloc: error path, the reason the record keeps
            return Record::incomplete(format!("its execution record is not whole: {why}"));
        }
        let Some((head, map)) = region.split_at_checked(HEADER) else {
            return Record::incomplete("its execution record is shorter than its header");
        };
        if head[..8] != MAGIC {
            return Record::incomplete("its execution record does not start with blaze's magic");
        }
        let slots = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
        let armed = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
        if armed != slots {
            // alloc: error path, the reason the record keeps
            return Record::incomplete(format!("the runtime armed {armed} of its {slots} slots"));
        }
        let Some(map) = map.get(..slots as usize) else {
            // alloc: error path, the reason the record keeps
            return Record::incomplete(format!("its execution record holds fewer than its {slots} slots"));
        };
        // alloc: once per run, the hits the record owns
        let mut hits = Vec::new();
        hits_of(map, &mut hits);
        Record::Complete { slots, hits }
    }

    /// Why it is not whole, for one that is not.
    pub fn why(&self) -> Option<&str> {
        match self {
            Record::Complete { .. } => None,
            Record::Incomplete { why } => Some(why),
        }
    }
}

const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
const HIGH: u64 = 0x8080_8080_8080_8080;

/// The indices of the bytes of `map` that are not zero, in order, added to `out`: eight bytes
/// a step, a word of zeros skipped in one compare, as most of a binary never runs in one test.
/// `map` is at most `u32::MAX` bytes.
pub fn hits_of(map: &[u8], out: &mut Vec<u32>) {
    let mut chunks = map.chunks_exact(8);
    let mut base = 0u32;
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        if word != 0 {
            // The high bit of each byte that is not zero: the low seven bits plus 0x7f carry
            // into it, and no byte carries into the next.
            let mut set = (((word & LOW) + LOW) | word) & HIGH;
            while set != 0 {
                out.push(base + set.trailing_zeros() / 8);
                set &= set - 1;
            }
        }
        base += 8;
    }
    for (at, byte) in chunks.remainder().iter().enumerate() {
        if *byte != 0 {
            out.push(base + at as u32);
        }
    }
}

#[cfg(test)]
mod tests;
