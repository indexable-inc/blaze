//! The sketches in a ring: one array of atomics its thread writes as each span closes
//! ([`closed`]), no lock and no read-modify-write, and any thread reads ([`listed`] and the
//! sketches' `merge_cells`).

use std::sync::atomic::{AtomicU64, Ordering, fence};

use blaze_graph::wait::WaitKind;

use super::{Cells, CountMin, DdSketch, Part, Shared, TopKeys};

/// Spans counted.
pub(crate) const SPANS: usize = 0;
pub(crate) const TOP_AT: usize = 1;
pub(crate) const COUNT_MIN_AT: usize = TOP_AT + TopKeys::CELLS;
pub(crate) const WAITS_AT: usize = COUNT_MIN_AT + CountMin::CELLS;
pub(crate) const KINDS: usize = WaitKind::ALL.len();
/// 17,512 counters, 137 KiB a ring.
pub(crate) const CELLS: usize = WAITS_AT + KINDS * DdSketch::CELLS;

/// A ring's counters, all 0.
pub(crate) fn cells() -> Box<[AtomicU64]> {
    // alloc: once per thread that records, its sketches for the process's life
    (0..CELLS).map(|_| AtomicU64::new(0)).collect()
}

/// The key of a span: its site and its `function`'s name.
pub(crate) fn key(site: u16, function: u32) -> u64 {
    u64::from(site) << 32 | u64::from(function)
}

/// The site and function of a [`key`].
pub(crate) fn of(key: u64) -> (u16, u32) {
    ((key >> 32) as u16, key as u32)
}

/// A span of `key` closed after `ticks`, on the thread that owns `cells`; `waits_on` its
/// wait's `WaitKind::index` plus one, 0 for a span that is no wait.
#[inline]
pub(crate) fn closed(cells: &[AtomicU64], key: u64, ticks: u64, waits_on: u64) {
    let mut cells = Shared(cells);
    cells.add(SPANS, 1);
    let estimate = CountMin::add_to(&mut Part { cells: &mut cells, from: COUNT_MIN_AT }, key, ticks);
    TopKeys::offer_to(&mut Part { cells: &mut cells, from: TOP_AT }, key, estimate, ticks);
    if let Some(kind) = waits_on.checked_sub(1).map(|it| it as usize).filter(|it| *it < KINDS) {
        DdSketch::add_to(&mut Part { cells: &mut cells, from: wait_at(kind) }, ticks);
    }
}

/// Where the DDSketch of the kind at `index` begins.
pub(crate) fn wait_at(index: usize) -> usize {
    WAITS_AT + index * DdSketch::CELLS
}

/// Each key a ring lists now, with its exact ticks since it entered the list: `None` when the
/// writer replaced the entry while it was read, so those ticks may be another key's.
pub(crate) fn listed(cells: &[AtomicU64], mut each: impl FnMut(u64, Option<u64>)) {
    let len = (cells[TOP_AT + TopKeys::LEN_AT].load(Ordering::Acquire) as usize).min(super::TOP);
    for entry in 0..len {
        let key = &cells[TOP_AT + TopKeys::key_at(entry)];
        let before = key.load(Ordering::Acquire);
        let exact = cells[TOP_AT + TopKeys::exact_at(entry)].load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let after = key.load(Ordering::Relaxed);
        match (before, after) {
            (0, 0) => {}
            (before, after) if before == after => each(after, Some(exact)),
            (_, 0) => each(before, None),
            (_, after) => each(after, None),
        }
    }
}
