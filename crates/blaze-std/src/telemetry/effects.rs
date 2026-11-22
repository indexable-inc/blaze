//! IO by effect kind: one row per [`Call`] kind (count, bytes, time, longest) at every level,
//! and from `Spans` a latency [`Dist`] per kind. Syncs ([`Call::Fsync`]) are their own row and
//! print on their own line: a wait for the device is never mixed with a read.

use core::alloc::Allocator;

use blaze_meter::{CALLS, Call};
use blaze_own::Vec;

use super::{Dist, TelemetryError};

/// One kind's effects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EffectRow {
    pub count: u64,
    pub bytes: u64,
    pub ns: u64,
    pub most_ns: u64,
}

impl EffectRow {
    pub const ZERO: EffectRow = EffectRow { count: 0, bytes: 0, ns: 0, most_ns: 0 };

    pub fn join(&mut self, other: &EffectRow) {
        self.count = self.count.wrapping_add(other.count);
        self.bytes = self.bytes.wrapping_add(other.bytes);
        self.ns = self.ns.wrapping_add(other.ns);
        if other.most_ns > self.most_ns {
            self.most_ns = other.most_ns;
        }
    }
}

/// A worker's effects.
pub struct Effects<A: Allocator + Copy> {
    rows: [EffectRow; CALLS],
    /// One per kind from `Spans`, none below.
    dists: Vec<Dist, A>,
}

impl<A: Allocator + Copy> Effects<A> {
    pub(super) fn new(keeps: bool, alloc: A) -> Result<Effects<A>, TelemetryError> {
        let most = if keeps { CALLS } else { 0 };
        let mut dists = Vec::try_with_capacity_in(most, alloc)
            .map_err(|_| TelemetryError::Reserve { what: "the effect distributions", bytes: most * size_of::<Dist>() })?;
        for _ in 0..most {
            dists.push(Dist::ZERO);
        }
        Ok(Effects { rows: [EffectRow::ZERO; CALLS], dists })
    }

    #[inline]
    pub(super) fn record(&mut self, kind: Call, bytes: u64, ns: u64) {
        let row = &mut self.rows[kind as usize];
        row.count += 1;
        row.bytes = row.bytes.wrapping_add(bytes);
        row.ns = row.ns.wrapping_add(ns);
        if ns > row.most_ns {
            row.most_ns = ns;
        }
        if let Some(dist) = self.dists.get_mut(kind as usize) {
            dist.record(ns);
        }
    }

    pub fn rows(&self) -> &[EffectRow; CALLS] {
        &self.rows
    }

    /// The latency distributions, one per kind in [`Call::ALL`]'s order; empty below `Spans`.
    pub fn dists(&self) -> &[Dist] {
        &self.dists
    }
}
