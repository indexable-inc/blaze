//! The kernel's entries as rows of a [`Trace`] (blaze-kernel, docs/v2/plan/design/kernel.md), so
//! [`crate::under`] and [`crate::over`] hold them to a build from scratch as they hold a query.
//!
//! An entry is one result: its key is its step's kind ([`kinds::KERNEL_ENTRIES`], `0x6800 +
//! step`) over the key the store keeps it under, its fingerprint is its value's hash, it ran
//! when the build computed it and did not when the store gave it. Its edges are the reads its
//! entry recorded, each a result of its own ([`kinds::KERNEL_READS`], `0x6c00 + kind`) whose
//! fingerprint is the hash the read saw. So against the trace of a build from scratch:
//!
//! - an entry the store gave whose read now hashes otherwise is [`crate::Stale::Unverified`]
//!   naming the read (lookup should have missed);
//! - one that read what the build from scratch read, with the same hashes, and holds other
//!   bytes is [`crate::Stale::UnrecordedRead`]: its step read something [`blaze_kernel::Reads`]
//!   did not record, or, for an entry with no reads (one kept by its whole identity, as codegen's
//!   dev templates are), its identity misses an input its bytes depend on (rule 21);
//! - one the build from scratch made under a key the incremental build never reached is
//!   [`crate::Stale::Absent`]: the incremental build keyed it otherwise, so its identity depends
//!   on the build's history.
//!
//! A row is keyed by [`whole::key`] of the entry's identity, the same for an entry with reads
//! and one without, so one identity is one row whatever shape its reads take. A caller that
//! keys its own entries (codegen's driver keeps dev templates under [`whole::key`] itself) gives
//! that key to [`entry`].

use std::fmt;

use blaze_cas::Key;
use blaze_ids::{ContentId, Fingerprint, Hasher};
use blaze_kernel::{Got, Identity, Read, Seen, StepId, whole};
use blaze_query::{Edge, QueryKey, QueryKind, kinds};

use crate::TraceBuilder;

/// The domain of a read's key in the trace: its kind and its key's bytes.
pub const READ_DOMAIN: &str = "blaze v2 proof kernel read 1";
/// The domain of an entry's value, whose first 128 bits are its fingerprint.
pub const VALUE_DOMAIN: &str = "blaze v2 proof kernel value 1";

/// A step or a read kind whose number has no kind in the trace: the ranges hold 1024 of each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unplaced {
    pub what: &'static str,
    pub number: u16,
}

impl fmt::Display for Unplaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let most = kinds::KERNEL_ENTRIES.end - kinds::KERNEL_ENTRIES.start;
        write!(
            f,
            "the kernel's {} {} has no kind in the oracle's trace, which places numbers below {most} \
             (blaze_query::kinds::KERNEL_ENTRIES and KERNEL_READS)",
            self.what, self.number
        )
    }
}

impl std::error::Error for Unplaced {}

fn placed(
    range: std::ops::Range<u16>,
    what: &'static str,
    number: u16,
) -> Result<QueryKind, Unplaced> {
    match range.start.checked_add(number) {
        Some(kind) if kind < range.end => Ok(QueryKind(kind)),
        _ => Err(Unplaced { what, number }),
    }
}

/// The step an entry row is of, `None` for a key that is no kernel entry's.
pub fn step_of(key: &QueryKey) -> Option<StepId> {
    kinds::KERNEL_ENTRIES
        .contains(&key.kind.0)
        .then(|| StepId(key.kind.0 - kinds::KERNEL_ENTRIES.start))
}

/// The row of the entry of `step` kept under `at`.
pub fn entry(step: StepId, at: Key) -> Result<QueryKey, Unplaced> {
    Ok(QueryKey { kind: placed(kinds::KERNEL_ENTRIES, "step", step.0)?, input: at.0 })
}

/// The row of `identity`'s entry, whatever its reads: [`entry`] of [`whole::key`].
pub fn row(identity: &Identity) -> Result<QueryKey, Unplaced> {
    entry(identity.step, whole::key(identity))
}

/// The row of `read`: its kind, and its key's bytes hashed.
pub fn read(read: Read<'_>) -> Result<QueryKey, Unplaced> {
    let kind = placed(kinds::KERNEL_READS, "read kind", read.kind.0)?;
    let mut hasher = Hasher::new(READ_DOMAIN);
    hasher.update(&read.kind.0.to_le_bytes()).part(read.key);
    Ok(QueryKey { kind, input: hasher.finish() })
}

/// The fingerprint of a value whose bytes are `bytes`.
pub fn value(bytes: &[u8]) -> Fingerprint {
    ContentId::of(VALUE_DOMAIN, bytes).fingerprint()
}

/// The fingerprint of what a read saw: its hash, zeros when the key named nothing.
pub fn seen(seen: Seen) -> Fingerprint {
    seen.hash().fingerprint()
}

/// Records one entry a build took from the store (`hit`) or computed: the row `row` with the
/// fingerprint `value`, an edge to each of `reads`, and each read a result of its own with the
/// hash it saw. A read is no rerun, so it is recorded as kept: [`crate::over`] counts entries,
/// never their reads.
pub fn record<'r>(
    trace: &mut TraceBuilder,
    row: QueryKey,
    value: Fingerprint,
    reads: impl IntoIterator<Item = (Read<'r>, Seen)>,
    hit: bool,
) -> Result<(), Unplaced> {
    // alloc: once an entry recorded, its edges; the trace builder copies them
    let mut edges = Vec::new();
    for (it, saw) in reads {
        let key = read(it)?;
        let saw = seen(saw);
        trace.insert(key, saw, &[], false);
        edges.push(Edge { saw, read: key });
    }
    trace.insert(row, value, &edges, !hit);
    Ok(())
}

/// [`record`] of what [`blaze_kernel::Kernel::get`] gave for `identity`: a hit with the reads its
/// entry recorded, a computed value with the reads it made.
pub fn record_got(
    trace: &mut TraceBuilder,
    identity: &Identity,
    got: &Got<'_>,
) -> Result<(), Unplaced> {
    let at = row(identity)?;
    match got {
        Got::Hit(entry) => record(trace, at, value(entry.value), entry.reads(), true),
        Got::Computed { value: bytes, reads, .. } => {
            record(trace, at, value(bytes), reads.iter(), false)
        }
    }
}
