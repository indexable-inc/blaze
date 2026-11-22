//! A distribution of every wait, by what it waited on (from `Spans`): the first
//! [`WAIT_DIST_ROWS`] names met hold rows, every later name is added to the `other` row. The
//! counts and sums by name at every level are the worker's `Tally` (`Waited`); this adds the
//! shape (median, p99, longest) beside them.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{Dist, TelemetryError};

/// The names a worker's table holds (16 rows of 544 bytes: 8.5 KiB, from `Spans`).
pub const WAIT_DIST_ROWS: usize = 16;

/// One name's waits.
#[derive(Clone, Copy, Debug)]
pub struct WaitDistRow {
    pub on: &'static str,
    pub count: u64,
    pub ns: u64,
    pub most: u64,
    pub dist: Dist,
}

impl WaitDistRow {
    pub const fn empty(on: &'static str) -> WaitDistRow {
        WaitDistRow { on, count: 0, ns: 0, most: 0, dist: Dist::ZERO }
    }

    #[inline]
    pub fn take(&mut self, ns: u64) {
        self.count += 1;
        self.ns = self.ns.wrapping_add(ns);
        if ns > self.most {
            self.most = ns;
        }
        self.dist.record(ns);
    }

    pub fn join(&mut self, other: &WaitDistRow) {
        self.count = self.count.wrapping_add(other.count);
        self.ns = self.ns.wrapping_add(other.ns);
        if other.most > self.most {
            self.most = other.most;
        }
        self.dist.plus(&other.dist);
    }
}

/// A worker's wait distributions.
pub struct Waits<A: Allocator + Copy> {
    rows: Vec<WaitDistRow, A>,
    other: WaitDistRow,
}

impl<A: Allocator + Copy> Waits<A> {
    pub(super) fn new(keeps: bool, alloc: A) -> Result<Waits<A>, TelemetryError> {
        let most = if keeps { WAIT_DIST_ROWS } else { 0 };
        let rows = Vec::try_with_capacity_in(most, alloc)
            .map_err(|_| TelemetryError::Reserve { what: "the wait distributions", bytes: most * size_of::<WaitDistRow>() })?;
        Ok(Waits { rows, other: WaitDistRow::empty("other") })
    }

    #[inline]
    pub(super) fn record(&mut self, on: &'static str, ns: u64) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.on == on) {
            row.take(ns);
            return;
        }
        if self.rows.len() < self.rows.capacity() {
            let mut row = WaitDistRow::empty(on);
            row.take(ns);
            self.rows.push(row);
            return;
        }
        self.other.take(ns);
    }

    pub fn rows(&self) -> &[WaitDistRow] {
        &self.rows
    }

    pub fn other(&self) -> &WaitDistRow {
        &self.other
    }
}
