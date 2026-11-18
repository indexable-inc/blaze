//! A test's reach as its run reports it: each cell fed in arrival order (the parent's publishes,
//! the child's entered slots, the sandbox's reads), each counted once however often it comes,
//! and [`Reach::finish`] gives the set sorted by `(kind, name, value)` with its digest, so two
//! arrival orders of one set give equal bytes (rule 25).
//!
//! Kept as one vector: a settled prefix, sorted and deduplicated, and an arrival tail. When the
//! tail outgrows the prefix (and [`MERGE_FLOOR`]) the whole is sorted and deduplicated again, so a
//! cell costs O(log n) amortised and nothing is hashed per first call. The cap counts distinct
//! cells, and the vector never holds more than about twice the cap: a run past it is refused by
//! name, never cut (rule 3).

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::{CellKind, Digest, MAX_READS, Read};

/// The arrival tail settled at the latest when it is this long, however small the prefix.
pub const MERGE_FLOOR: usize = 64;

pub(crate) const REACH_DOMAIN: &str = "blaze-test-reach reach digest 1";

/// The bytes of one row of the record (record.rs, "Its record" in tests-stage.md): the kind, seven
/// zero bytes, the name, the value.
pub const ROW: usize = 72;

/// What a [`Reach`] is made with (rule 83): both are choices.
pub struct ReachOpts<A: Allocator> {
    /// The most distinct cells; at most [`MAX_READS`].
    pub cap: usize,
    pub alloc: A,
}

/// Why a reach could not be kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReachError {
    /// More distinct cells than the cap.
    Over { cap: usize },
    /// A cap above [`MAX_READS`] was asked: no record could hold the reach.
    CapAboveMax { cap: usize },
    /// One cell with two values in one run (a file read twice that moved between): rule 21. The
    /// run's result is filed not reusable ([`crate::NotReusable::CellMovedDuringRun`]).
    TwoValues { kind: CellKind, name: Digest, first: Digest, second: Digest },
}

impl fmt::Display for ReachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReachError::Over { cap } => write!(f, "a test reached more than {cap} distinct cells (blaze-test-reach reach.rs `Reach::add`)"),
            ReachError::CapAboveMax { cap } => write!(f, "a reach's cap of {cap} is above the record's {MAX_READS} (blaze-test-reach reach.rs `Reach::new`)"),
            ReachError::TwoValues { kind, name, first, second } => write!(
                f,
                "{} {name} answered two values in one run, {first} and {second}: the test's result cannot be kept by its reach (rule 21; blaze-test-reach reach.rs)",
                kind.word()
            ),
        }
    }
}

/// The reach being collected for one test's run.
pub struct Reach<A: Allocator> {
    rows: Vec<Read, A>,
    /// `rows[..settled]` is sorted, each cell once.
    settled: usize,
    cap: usize,
}

impl<A: Allocator> Reach<A> {
    pub fn new(opts: ReachOpts<A>) -> Result<Reach<A>, ReachError> {
        if opts.cap > MAX_READS {
            return Err(ReachError::CapAboveMax { cap: opts.cap });
        }
        Ok(Reach { rows: Vec::new_in(opts.alloc), settled: 0, cap: opts.cap })
    }

    /// `read` came: counted once however often it comes.
    pub fn add(&mut self, read: Read) -> Result<(), ReachError> {
        self.rows.push(read);
        let tail = self.rows.len() - self.settled;
        if tail > self.settled.max(MERGE_FLOOR) {
            self.settle()?;
        }
        Ok(())
    }

    /// The whole sorted, each cell once, a name with two values refused, the cap checked.
    fn settle(&mut self) -> Result<(), ReachError> {
        self.rows.sort_unstable();
        self.rows.dedup();
        check_one_value(&self.rows)?;
        if self.rows.len() > self.cap {
            return Err(ReachError::Over { cap: self.cap });
        }
        self.settled = self.rows.len();
        Ok(())
    }

    /// The reach: sorted, each cell once, with its digest.
    pub fn finish(mut self) -> Result<Reached<A>, ReachError> {
        self.settle()?;
        let digest = digest_of(&self.rows);
        Ok(Reached { rows: self.rows, digest })
    }
}

/// A finished reach: the rows sorted by `(kind, name, value)`, each once, one value a name.
pub struct Reached<A: Allocator> {
    rows: Vec<Read, A>,
    digest: Digest,
}

impl<A: Allocator> Reached<A> {
    /// No cell: an ignored test's, or a run whose reach was refused ([`ReachError::TwoValues`])
    /// and is filed not reusable.
    pub fn empty(alloc: A) -> Reached<A> {
        Reached { rows: Vec::new_in(alloc), digest: digest_of(&[]) }
    }

    pub fn reads(&self) -> &[Read] {
        &self.rows
    }

    pub fn digest(&self) -> Digest {
        self.digest
    }
}

/// Sorted `rows` with one name holding two values is refused, naming the first such.
pub(crate) fn check_one_value(rows: &[Read]) -> Result<(), ReachError> {
    for pair in rows.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.kind == b.kind && a.name == b.name {
            return Err(ReachError::TwoValues { kind: a.kind, name: a.name, first: a.value, second: b.value });
        }
    }
    Ok(())
}

/// One row's bytes.
pub(crate) fn row_bytes(read: &Read) -> [u8; ROW] {
    let mut row = [0u8; ROW];
    row[0] = read.kind as u8;
    row[8..40].copy_from_slice(&read.name.0);
    row[40..72].copy_from_slice(&read.value.0);
    row
}

/// The digest of sorted rows: every row's bytes in order, under the reach's domain.
pub(crate) fn digest_of(rows: &[Read]) -> Digest {
    let mut hasher = blaze_blake3::Hasher::new_derive_key(REACH_DOMAIN);
    for read in rows {
        hasher.update(&row_bytes(read));
    }
    Digest(hasher.finalize())
}
