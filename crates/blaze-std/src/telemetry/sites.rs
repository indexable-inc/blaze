//! Allocation by site (`Detail`): per site, the allocations, their bytes and a [`Dist`] of
//! their sizes. A site is the name of the task the allocating thread runs (`blaze_meter::doing`,
//! which the task body sets already): the global allocator has no context, so a site finer
//! than the task is not known to it.
//!
//! The feed (a hook, listed for its owner): the counting allocator, when `Detail` is on, adds
//! each allocation to its thread's site rows in blaze-meter's thread-local cells (a probe by the
//! `doing` pointer over 8 rows, then a bucket add: 5 to 10 ns), and the task's end moves the
//! thread's rows into its worker's [`Sites`] ([`Sites::allocated`] per row).
//!
//! Sampled stacks for allocations (the stack of one allocation in N) are design only: the
//! sampler's stack walk ([`super::sampler`]) is the piece they would reuse.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{Dist, TelemetryError};

/// The sites a worker keeps (32 rows of 536 bytes: 17 KiB, from `Detail`).
pub const SITE_ROWS: usize = 32;

/// One site's allocations.
#[derive(Clone, Copy, Debug)]
pub struct SiteRow {
    pub site: &'static str,
    pub count: u64,
    pub bytes: u64,
    pub sizes: Dist,
}

impl SiteRow {
    pub const fn empty(site: &'static str) -> SiteRow {
        SiteRow { site, count: 0, bytes: 0, sizes: Dist::ZERO }
    }

    pub fn join(&mut self, other: &SiteRow) {
        self.count = self.count.wrapping_add(other.count);
        self.bytes = self.bytes.wrapping_add(other.bytes);
        self.sizes.plus(&other.sizes);
    }
}

/// A worker's sites.
pub struct Sites<A: Allocator + Copy> {
    rows: Vec<SiteRow, A>,
    other: SiteRow,
}

impl<A: Allocator + Copy> Sites<A> {
    pub(super) fn new(keeps: bool, alloc: A) -> Result<Sites<A>, TelemetryError> {
        let most = if keeps { SITE_ROWS } else { 0 };
        let rows = Vec::try_with_capacity_in(most, alloc)
            .map_err(|_| TelemetryError::Reserve { what: "the allocation sites", bytes: most * size_of::<SiteRow>() })?;
        Ok(Sites { rows, other: SiteRow::empty("other") })
    }

    /// One allocation of `bytes` at `site`.
    #[inline]
    pub fn allocated(&mut self, site: &'static str, bytes: u64) {
        let row = match self.rows.iter().position(|row| row.site == site) {
            Some(at) => &mut self.rows[at],
            None if self.rows.len() < self.rows.capacity() => {
                self.rows.push(SiteRow::empty(site));
                let last = self.rows.len() - 1;
                &mut self.rows[last]
            }
            None if self.rows.capacity() == 0 => return,
            None => &mut self.other,
        };
        row.count += 1;
        row.bytes = row.bytes.wrapping_add(bytes);
        row.sizes.record(bytes);
    }

    pub fn rows(&self) -> &[SiteRow] {
        &self.rows
    }

    pub fn other(&self) -> &SiteRow {
        &self.other
    }
}
