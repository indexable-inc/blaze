//! The loop map's one builder (backend-next.md D100 points 2 and 11): every header's live rows,
//! found in any order by the analysis pass, handed on sorted by key with each key once, which is
//! the record's own rule (blaze-tier record/form.rs `check`: "each header's live values by value",
//! refused as "live values not sorted by value, each once").
//!
//! ONE KEY SPACE. A row's `value` is its key. A value row's key is the value's LIR index
//! (`0..values`); an address-taken stack slot's row ([`Place::Slot`]) is keyed AFTER every value:
//! `values + slot` ([`slot_key`], read back by [`slot_of`]). Keying a slot by its bare index put
//! two kinds of thing under one key (slot 0 and value 0 in one header: the tier equality
//! harness's seed 63, tape `0 2 0*2 1`, where the entry block's three `slot_addr` results are
//! values 0 to 2 and their slots 0 to 2 are taken in the same block): the record refused it, and
//! no sort can make such a list "each once". With one key space a header's list is sorted and
//! unique by construction; the reader (the loop-entry variant, compiled from the same LIR) knows
//! `values` and reads a slot's index back.
//!
//! Two rows of one header under one key are merged when their places agree (a value reached by
//! several walks) and refused by name when they do not: a value has ONE place at a header (its
//! home), so two places is a broken analysis, never a choice to make here.

use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Live, LoopHeader, Place, TierError, TierName};

/// The key of slot `slot`'s row in a function of `values` instructions.
pub(crate) const fn slot_key(values: u32, slot: u32) -> u32 {
    values + slot
}

/// The slot index a [`Place::Slot`] row's key names in a function of `values` instructions, or
/// `None` for a key below the slots' range.
pub(crate) fn slot_of(values: usize, key: u32) -> Option<usize> {
    (key as usize).checked_sub(values)
}

fn refused(what: &'static str) -> TierError {
    TierError::Refused { tier: TierName::Fast, inst: None, what }
}

/// The rows found so far, `(header, row)`, in any order.
pub(crate) struct LoopMap<'a> {
    values: u32,
    slots: u32,
    headers: u32,
    rows: AVec<'a, (u32, Live)>,
}

impl<'a> LoopMap<'a> {
    /// An empty map for a function of `values` instructions and `slots` stack slots with
    /// `headers` loop headers. Refuses a function whose instructions and slots together do not
    /// fit one `u32` key space.
    pub(crate) fn new(values: usize, slots: usize, headers: usize, alloc: Alloc<'a>) -> Result<LoopMap<'a>, TierError> {
        let fits = values.checked_add(slots).is_some_and(|it| it < u32::MAX as usize) && headers < u32::MAX as usize;
        if !fits {
            return Err(TierError::TooLarge {
                tier: TierName::Fast,
                what: "the function's instructions and stack slots as one loop-map key space",
            });
        }
        Ok(LoopMap { values: values as u32, slots: slots as u32, headers: headers as u32, rows: Vec::new_in(alloc) })
    }

    /// Value `value` is live into header `header` at `place` (its home there: a register or its
    /// cell).
    pub(crate) fn value(&mut self, header: u32, value: u32, place: Place) -> Result<(), TierError> {
        if value >= self.values {
            return Err(refused("a loop-map value past the function's instructions"));
        }
        if let Place::Slot { .. } = place {
            return Err(refused("a loop-map value row with a stack slot's place"));
        }
        self.push(header, Live { value, place })
    }

    /// Slot `slot`, whose address is taken, is live into header `header`: it stays in the fast
    /// frame (D100 point 2); its `sp_offset` is the emission pass's to fill.
    pub(crate) fn slot(&mut self, header: u32, slot: u32) -> Result<(), TierError> {
        if slot >= self.slots {
            return Err(refused("a loop-map slot the function lacks"));
        }
        self.push(header, Live { value: slot_key(self.values, slot), place: Place::Slot { sp_offset: 0 } })
    }

    fn push(&mut self, header: u32, live: Live) -> Result<(), TierError> {
        if header >= self.headers {
            return Err(refused("a loop-map row of a header the function lacks"));
        }
        self.rows.push((header, live));
        Ok(())
    }

    /// One [`LoopHeader`] per header in index order (`at` 0) and the live rows: each header's
    /// sorted by key, each key once.
    pub(crate) fn finish(mut self, alloc: Alloc<'a>) -> Result<(AVec<'a, LoopHeader>, AVec<'a, Live>), TierError> {
        self.rows.sort_unstable_by_key(|&(header, live)| (header, live.value));
        let mut loops: AVec<'a, LoopHeader> = Vec::with_capacity_in(self.headers as usize, alloc);
        let mut live: AVec<'a, Live> = Vec::with_capacity_in(self.rows.len(), alloc);
        let mut next = 0usize;
        for header in 0..self.headers {
            let first = live.len() as u32;
            while next < self.rows.len() && self.rows[next].0 == header {
                let row = self.rows[next].1;
                next += 1;
                let last = match live.len() as u32 > first {
                    true => live.last().copied(),
                    false => None,
                };
                match last {
                    Some(last) if last.value == row.value => {
                        if last.place != row.place {
                            return Err(refused("one value live into a loop header at two places"));
                        }
                    }
                    _ => live.push(row),
                }
            }
            loops.push(LoopHeader { at: 0, first, count: live.len() as u32 - first });
        }
        Ok((loops, live))
    }
}
