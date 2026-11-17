//! The code area: address space reserved once, handed out from its end and never again.
//!
//! The invariant: an offset is handed out at most once. A program counter in the area
//! therefore names one body for the life of the process, whatever was freed since: a profile
//! sample, a backtrace or an unwind lookup taken at any time is still right later
//! (docs/v2/plan/design/launcher.md, section 1). [`Area::append`] upholds it by only ever
//! moving `end` up; the simulation (`tests/sim.rs`) checks it on every range the core hands a
//! driver.
//!
//! What a freed range gives back is memory, not addresses: [`Area::free`] names each whole page
//! that no live range overlaps any more and that the end has passed, once.

use alloc::vec::Vec;

use crate::{Error, Kept, Range};

/// The code area of one process, as offsets from where its driver mapped it.
pub struct Area {
    /// The reservation's length.
    limit: u64,
    /// The page size: a power of two.
    page: u64,
    /// Where the next range goes. Everything below was handed out or skipped for alignment.
    end: u64,
    /// By page number, for each page below `end`: how many ranges not yet freed overlap it.
    live: Vec<u32, Kept>,
}

impl Area {
    /// An empty area of `limit` bytes in pages of `page` bytes.
    pub fn new(limit: u64, page: u64, alloc: Kept) -> Result<Area, Error> {
        if !page.is_power_of_two() {
            return Err(Error::Config { what: "the page size is not a power of two", value: page });
        }
        if limit == 0 || limit % page != 0 {
            return Err(Error::Config {
                what: "the area's length is not a whole number of pages",
                value: limit,
            });
        }
        Ok(Area { limit, page, end: 0, live: Vec::new_in(alloc) })
    }

    /// Where the next range would begin.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// The bytes not yet handed out.
    pub fn room(&self) -> u64 {
        self.limit - self.end
    }

    /// Where a range of `len` bytes aligned to `align` would go after `end`, and where it
    /// would end: `None` when the area has no room for it. Changes nothing, so a caller that
    /// must place several ranges or none asks for each in turn first.
    pub fn plan(&self, end: u64, len: u64, align: u64) -> Option<(u64, u64)> {
        let at = end.checked_add(align - 1)? & !(align - 1);
        let after = at.checked_add(len)?;
        (after <= self.limit).then_some((at, after))
    }

    /// Hands out the next `len` bytes aligned to `align` (a power of two; `len` is not zero:
    /// the core checks both before it asks).
    pub fn append(&mut self, len: u64, align: u64) -> Result<Range, Error> {
        let Some((at, after)) = self.plan(self.end, len, align) else {
            return Err(Error::Full { need: len, room: self.room() });
        };
        let last = (after - 1) / self.page;
        while (self.live.len() as u64) <= last {
            self.live.push(0);
        }
        for page in at / self.page..=last {
            self.live[page as usize] += 1;
        }
        self.end = after;
        Ok(Range { at, len })
    }

    /// Frees `range`, one [`Area::append`] handed out and that was not freed before (the core
    /// frees a body once: its state says so). Calls `gone` with each run of whole pages that
    /// now hold nothing live and that the end has passed: the driver gives them back to the
    /// OS. A page the end is still in is not named; it is when a later range in it is freed
    /// (a page the end then leaves through alignment alone, with no later range in it, stays
    /// committed: one page, and only when the end was within an alignment of the page's end).
    pub fn free(&mut self, range: Range, mut gone: impl FnMut(Range)) {
        let first = range.at / self.page;
        let last = (range.end() - 1) / self.page;
        let mut run: Option<u64> = None;
        for page in first..=last {
            let count = &mut self.live[page as usize];
            *count -= 1;
            let empty = *count == 0 && (page + 1) * self.page <= self.end;
            match (empty, run) {
                (true, None) => run = Some(page),
                (false, Some(from)) => {
                    gone(Range { at: from * self.page, len: (page - from) * self.page });
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(from) = run {
            gone(Range { at: from * self.page, len: (last + 1 - from) * self.page });
        }
    }
}
