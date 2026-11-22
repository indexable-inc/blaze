//! The Tests stage's core (docs/v2/plan/design/tests-stage.md, plan row E2): a test's result is a
//! record keyed by the test and valid by its REACH, the cells its process read (each code record
//! it reached on a first call, each static's datum it was sent, each file and variable the sandbox
//! let it read), so an edit reruns exactly the tests whose reach holds a moved cell.
//!
//! | module | what |
//! |---|---|
//! | [`reach`] | [`Reach`]: fed each cell in arrival order, each counted once; [`Reached`]: sorted, with its digest |
//! | [`record`] | [`TestRecord`]: the result record's bytes, encoded, and opened with every limit checked |
//! | [`decide`] | [`Reusable`] from a test's grants, [`enter`]: reuse or rerun naming the first moved cell, [`audit`] |
//! | [`impact`] | [`ImpactIndex`]: CSR inverted change-impact index for `O(|changed| * (log U + fanout))` selection |
//! | [`plan`] | [`ExecutionPlan`]: failed-last + LPT test scheduler, and [`advise_tier_up`]: JIT tier-up advisor |
//!
//! Nothing here touches the system, reads a clock or picks an allocator (rule 75): the driver
//! feeds cells in, answers "has this cell moved", and writes the bytes it is handed.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod decide;
pub mod impact;
pub mod persist;
pub mod plan;
pub mod reach;
pub mod record;

#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;

use core::fmt;

pub use decide::{Audit, Because, Enter, Grants, NotReusable, Reusable, audit, enter, enter_stored};
pub use impact::{CodeSymbols, ImpactIndex, Posting, SelectedTest, Selection};
pub use persist::{ImpactView, PersistError, encode_impact};
pub use plan::{ExecutionPlan, PlannedTest, TestHistory, TierAdvice, TierOpts, advise_tier_up, advise_tier_up_for_plan};
pub use reach::{Reach, ReachError, ReachOpts, Reached};
pub use record::{Out, RecordError, TestRecord};

/// The most cells one test's reach holds: a million first calls, files and variables. Past it
/// the run is refused by name ([`ReachError::Over`]), never cut.
pub const MAX_READS: usize = 1 << 20;

/// A 32-byte digest or key, as the store and the run wire have them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// All zero: an unset variable's value, a passing record's output key.
    pub const ZERO: Digest = Digest([0; 32]);

    /// `bytes` hashed under `domain` (BLAKE3's derive-key mode: no two domains collide).
    pub fn of(domain: &str, bytes: &[u8]) -> Digest {
        Digest(blaze_blake3::derive_key(domain, bytes))
    }
}

impl fmt::Display for Digest {
    /// The first 12 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// What kind of cell a test read. Discriminants are the record's, and the record's rows sort by
/// them first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum CellKind {
    /// An instance asked for on a first call: name its symbol, value its code key.
    Code = 0,
    /// A static's data record sent to the process: name its symbol, value the datum's record id.
    Datum = 1,
    /// A file the sandbox let it read: name H(path from the workspace root), value H(bytes).
    File = 2,
    /// An environment variable it read: name H(name), value H(value), [`Digest::ZERO`] if unset.
    Env = 3,
}

impl CellKind {
    pub const ALL: [CellKind; 4] = [CellKind::Code, CellKind::Datum, CellKind::File, CellKind::Env];

    /// The kind whose byte is `byte`; `None` for any other byte.
    pub const fn from_byte(byte: u8) -> Option<CellKind> {
        match byte {
            0 => Some(CellKind::Code),
            1 => Some(CellKind::Datum),
            2 => Some(CellKind::File),
            3 => Some(CellKind::Env),
            _ => None,
        }
    }

    pub const fn word(self) -> &'static str {
        match self {
            CellKind::Code => "code",
            CellKind::Datum => "static",
            CellKind::File => "file",
            CellKind::Env => "variable",
        }
    }
}

/// One cell a test read and what it answered: a row of its reach. Ordered by kind, then name,
/// then value: the record's order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Read {
    pub kind: CellKind,
    pub name: Digest,
    pub value: Digest,
}

impl Read {
    pub const fn code(symbol: [u8; 32], key: [u8; 32]) -> Read {
        Read { kind: CellKind::Code, name: Digest(symbol), value: Digest(key) }
    }

    pub const fn datum(symbol: [u8; 32], record: [u8; 32]) -> Read {
        Read { kind: CellKind::Datum, name: Digest(symbol), value: Digest(record) }
    }

    /// A file by its path from the workspace root (never an absolute path: rule 25) and bytes.
    pub fn file(path_from_root: &[u8], bytes: &[u8]) -> Read {
        Read { kind: CellKind::File, name: Digest::of(FILE_NAME_DOMAIN, path_from_root), value: Digest::of(FILE_VALUE_DOMAIN, bytes) }
    }

    /// A variable by its name and its value, `None` when it was unset (a read all the same).
    pub fn env(name: &[u8], value: Option<&[u8]>) -> Read {
        let value = match value {
            Some(bytes) => Digest::of(ENV_VALUE_DOMAIN, bytes),
            None => Digest::ZERO,
        };
        Read { kind: CellKind::Env, name: Digest::of(ENV_NAME_DOMAIN, name), value }
    }
}

impl fmt::Display for Read {
    /// `blaze why`'s words for the cell and what it answered then: `code 0123456789ab (was
    /// ba9876543210)`. The driver names the cell in source words (the instance's path, the file's
    /// path) where it has them; this is the form with nothing but the record.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} (was {})", self.kind.word(), self.name, self.value)
    }
}

const FILE_NAME_DOMAIN: &str = "blaze-test-reach file path 1";
const FILE_VALUE_DOMAIN: &str = "blaze-test-reach file bytes 1";
const ENV_NAME_DOMAIN: &str = "blaze-test-reach env name 1";
const ENV_VALUE_DOMAIN: &str = "blaze-test-reach env value 1";
