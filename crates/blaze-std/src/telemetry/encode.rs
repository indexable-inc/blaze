//! The report's stored form: deterministic CBOR (blaze-cbor's writers), positional arrays
//! only, so there is no map and no map order; every row in the order [`Report::finish`] put it
//! in (by key); no host path, no time of day (times are durations or nanoseconds from the
//! run's start on the world's clock), no address of this process (the profile's functions are
//! names; an address outside the symbol table is written as its offset from the image's start).
//!
//! ```text
//! report = [ENCODING, level, exit, merges, open_spans,
//!           cells, waited, reconciled / null,
//!           [worker...], [effect...], [span...], other_span, [wait...], other_wait,
//!           chain / null, [site..., other_site], profile / null]
//! cells = [[count; COUNTS], [size; SIZES], [frame bytes; TAGS], [frame resets; TAGS], [call; CALLS], [call ns; CALLS]]
//! dist = [[bucket, count]...]      (non-zero buckets, ascending)
//! brief = [n; FIELDS]
//! ```

use core::alloc::Allocator;

use blaze_cbor::{array, boolean, null, text, uint};
use blaze_meter::{Call, Cells, Waited};
use blaze_own::Vec;

use super::{Brief, ChainReport, Dist, Report, SpanPath, SpanRow, TelemetryError, Then, WaitDistRow, sampler::Profile};

/// The encoding's version: the first item of every report.
pub const ENCODING: u64 = 1;

fn opt<B: Allocator>(out: &mut Vec<u8, B>, n: Option<u64>) {
    match n {
        Some(n) => uint(out, n),
        None => null(out),
    }
}

fn uints<B: Allocator>(out: &mut Vec<u8, B>, ns: &[u64]) {
    array(out, ns.len() as u64);
    for n in ns {
        uint(out, *n);
    }
}

pub(super) fn dist<B: Allocator>(out: &mut Vec<u8, B>, dist: &Dist) {
    let held = dist.buckets().iter().filter(|n| **n > 0).count();
    array(out, held as u64);
    for (at, n) in dist.buckets().iter().enumerate() {
        if *n > 0 {
            array(out, 2);
            uint(out, at as u64);
            uint(out, *n);
        }
    }
}

fn brief<B: Allocator>(out: &mut Vec<u8, B>, brief: &Brief) {
    uints(out, &brief.v);
}

fn cells<B: Allocator>(out: &mut Vec<u8, B>, cells: &Cells) {
    array(out, 6);
    uints(out, &cells.counts);
    uints(out, &cells.sizes);
    uints(out, &cells.frame_bytes);
    uints(out, &cells.frame_resets);
    uints(out, &cells.calls);
    uints(out, &cells.call_ns);
}

fn waited<B: Allocator>(out: &mut Vec<u8, B>, waited: &Waited) {
    // Rows by name, so the tally's own row order (first met) never reaches the bytes.
    let mut rows = [None; blaze_meter::WAIT_ROWS];
    let mut held = 0;
    for row in waited.rows.iter().flatten() {
        rows[held] = Some(*row);
        held += 1;
    }
    let rows = &mut rows[..held];
    rows.sort_unstable_by(|a, b| match (a, b) {
        (Some(a), Some(b)) => a.on.cmp(b.on).then(a.id.cmp(&b.id)),
        _ => core::cmp::Ordering::Equal,
    });
    array(out, 3);
    array(out, held as u64);
    for row in rows.iter().flatten() {
        array(out, 4);
        text(out, row.on);
        opt(out, row.id);
        uint(out, row.count);
        uint(out, row.ns);
    }
    uint(out, waited.other_count);
    uint(out, waited.other_ns);
}

fn path<B: Allocator>(out: &mut Vec<u8, B>, path: &SpanPath) {
    array(out, 2);
    array(out, path.names().len() as u64);
    for name in path.names() {
        text(out, name);
    }
    boolean(out, path.deeper());
}

fn span<B: Allocator>(out: &mut Vec<u8, B>, row: &SpanRow) {
    array(out, 6);
    path(out, &row.path);
    uint(out, row.count);
    uint(out, row.wall_ns);
    uint(out, row.wall_most);
    brief(out, &row.sum);
    brief(out, &row.most);
}

fn wait<B: Allocator>(out: &mut Vec<u8, B>, row: &WaitDistRow) {
    array(out, 5);
    text(out, row.on);
    uint(out, row.count);
    uint(out, row.ns);
    uint(out, row.most);
    dist(out, &row.dist);
}

fn chain<B: Allocator>(out: &mut Vec<u8, B>, chain: &ChainReport) {
    let links = chain.links.iter().flatten().count();
    array(out, 5);
    uint(out, chain.total_ns);
    uint(out, u64::from(chain.len));
    uint(out, chain.partial.tasks_past);
    uint(out, chain.partial.waits_past);
    array(out, links as u64);
    for link in chain.links.iter().flatten() {
        array(out, 5);
        uint(out, link.task);
        text(out, link.name);
        uint(out, link.own_ns);
        uint(out, link.value_ns);
        match link.then {
            Then::Nothing => null(out),
            Then::Task(id) => {
                array(out, 2);
                uint(out, 0);
                uint(out, id);
            }
            Then::Wait { on, ns } => {
                array(out, 3);
                uint(out, 1);
                text(out, on);
                uint(out, ns);
            }
        }
    }
}

impl<A: Allocator + Copy> Report<A> {
    /// The report's bytes appended to `out`. Refused before [`Report::finish`].
    pub fn encode_into<B: Allocator>(&self, out: &mut Vec<u8, B>) -> Result<(), TelemetryError> {
        if !self.finished() {
            return Err(TelemetryError::Unfinished);
        }
        array(out, 17);
        uint(out, ENCODING);
        uint(out, self.level.code());
        opt(out, self.exit.map(|exit| exit.code()));
        uint(out, u64::from(self.merges));
        uint(out, self.open_spans);
        cells(out, &self.cells);
        waited(out, &self.waited);
        match &self.reconciled {
            Some(r) => {
                array(out, 8);
                uint(out, r.wall_ns);
                uint(out, u64::from(r.workers));
                uint(out, r.lifetime_ns);
                uint(out, r.accounted_ns);
                uint(out, r.working_ns);
                opt(out, r.process_cpu_ns);
                opt(out, r.span_cpu_ns);
                opt(out, r.most_resident);
            }
            None => null(out),
        }
        array(out, self.workers.len() as u64);
        for row in self.workers.iter() {
            let p = &row.pool;
            array(out, 10);
            uint(out, u64::from(row.worker));
            uint(out, p.working_ns);
            uint(out, p.parked_ns);
            uint(out, p.idle_ns);
            uint(out, p.tasks);
            uint(out, p.failed);
            match p.longest {
                Some(longest) => {
                    array(out, 2);
                    text(out, longest.name);
                    uint(out, longest.ns);
                }
                None => null(out),
            }
            opt(out, p.lifetime());
            uint(out, row.open_spans);
            let samples = p.samples().count();
            array(out, samples as u64);
            for sample in p.samples() {
                array(out, 3);
                uint(out, sample.at_ns);
                uint(out, u64::from(sample.depth));
                opt(out, sample.idle.map(|idle| idle.code()));
            }
        }
        array(out, Call::ALL.len() as u64);
        for call in Call::ALL {
            let row = &self.effects[call as usize];
            array(out, 5);
            uint(out, row.count);
            uint(out, row.bytes);
            uint(out, row.ns);
            uint(out, row.most_ns);
            dist(out, &self.effect_dists[call as usize]);
        }
        array(out, self.spans.len() as u64);
        for row in self.spans.iter() {
            span(out, row);
        }
        span(out, &self.other_span);
        array(out, self.waits.len() as u64);
        for row in self.waits.iter() {
            wait(out, row);
        }
        wait(out, &self.other_wait);
        match &self.chain {
            Some(c) => chain(out, c),
            None => null(out),
        }
        // The sites, then the `other` row last.
        array(out, self.sites.len() as u64 + 1);
        for row in self.sites.iter().chain(core::iter::once(&self.other_site)) {
            array(out, 4);
            text(out, row.site);
            uint(out, row.count);
            uint(out, row.bytes);
            dist(out, &row.sizes);
        }
        match &self.profile {
            Some(profile) => encode_profile(out, profile),
            None => null(out),
        }
        Ok(())
    }
}

fn encode_profile<B: Allocator>(out: &mut Vec<u8, B>, profile: &Profile) {
    profile.encode_into(out);
}
