//! Attribution (docs/v2/plan/design/backend-next.md, D100 point 4): the ring's raw entries turned
//! into piece 0's [`Profile`] (samples by function, sampled call edges) and the first-call order
//! into its [`Startup`]. Sans-IO: the caller drains the rings and hands the entries over with the
//! placed code's ranges as they were while the entries were taken; nothing here reads memory.
//!
//! **The leaf** is the placed range holding the entry's program counter. Ranges are given whole and
//! checked once ([`Placed::from_ranges`]: sorted, none empty, none overlapping), then found by a
//! binary search per entry.
//!
//! **The caller** (D100 point 4, the architect's objection): every tier keeps an x29 frame record,
//! so the caller is the return address in the frame record, `[x29 + 8]` (the sampler's
//! `frame_ret`), EXCEPT
//! - in the prologue's first two words (`stp x29, x30, [sp, #-n]!` then `mov x29, sp`): x29 still
//!   names the caller's own frame there, whose record holds the caller's caller; and
//! - at a `ret` word: the epilogue has already loaded the caller's x29 back;
//!
//! where it is the link register. In the body the link register holds the return address of the
//! last call the function itself made, which points into the function: taking it there is the bug
//! [`Plant::LrAlways`] plants. A return address names the instruction after the `bl`, which may lie
//! one past the caller's end (a call as its last word), so the caller is the range holding the
//! address minus one word.
//!
//! **AArch64 only.** The words and the prologue's length are AArch64's; run mode places AArch64
//! code only (run_mode.rs refuses x86-64 by name), and an x86-64 attribution (the return address at
//! `[rsp]` at a `ret`, a one-byte `push rbp`) is a different rule, not this one with other numbers.
//!
//! **Nothing quietly dropped**: an entry whose program counter is in no placed range, one with no
//! caller to read (the frame check refused, so `frame_ret` is 0), and one whose caller is outside
//! placed code (the launcher, a C library) are each counted in the [`Summary`] the caller reports
//! beside the profile.
//!
//! Records hold symbols only, never an address or a time (piece 0's sample.rs, point 6); rows are
//! sorted by symbol and edges by (caller, callee), each unique: a function of the multiset of
//! entries, whatever order the rings gave them in.

use core::fmt;

use blaze_code::record::Symbol;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::sample::{EdgeRow, Profile, ProfileRow, SampleEntry, Startup};

/// The bytes from a function's entry in which x29 still names the caller's frame: the prologue's
/// `stp x29, x30, [sp, #-n]!` and `mov x29, sp` (blaze-emit's prologue, every tier's).
pub const PROLOGUE_BYTES: u64 = 8;

/// One AArch64 instruction word: a return address minus this is the `bl` that made it.
const WORD: u64 = 4;

/// Whether `word` is a return: `ret Xn` (`0xd65f0000 | n << 5`), or the pointer-authenticating
/// `retaa` / `retab`, after which the link register holds the return address too.
pub const fn is_ret(word: u32) -> bool {
    word & 0xffff_fc1f == 0xd65f_0000 || word == 0xd65f_0bff || word == 0xd65f_0fff
}

/// One placed function's code: `low..high` in the run's address space, and its symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub low: u64,
    pub high: u64,
    pub symbol: Symbol,
}

/// The placed ranges of one sampling pass, sorted by `low`, none empty, none overlapping.
pub struct Placed<'a> {
    ranges: AVec<'a, Range>,
}

impl<'a> Placed<'a> {
    /// The ranges, checked: sorted here (by `low`), then refused by name if one is empty or two
    /// overlap.
    pub fn from_ranges(mut ranges: AVec<'a, Range>) -> Result<Placed<'a>, AttributeError> {
        ranges.sort_unstable_by_key(|range| (range.low, range.high, range.symbol));
        let mut last: Option<Range> = None;
        for &range in ranges.iter() {
            if range.high <= range.low {
                return Err(AttributeError::EmptyRange { symbol: range.symbol, low: range.low, high: range.high });
            }
            if let Some(before) = last
                && range.low < before.high
            {
                return Err(AttributeError::Overlap { first: before.symbol, second: range.symbol, at: range.low });
            }
            last = Some(range);
        }
        Ok(Placed { ranges })
    }

    /// The range holding `pc`.
    pub fn find(&self, pc: u64) -> Option<&Range> {
        // The first range whose `low` is past `pc`; the one before it is the only candidate.
        let after = self.ranges.partition_point(|range| range.low <= pc);
        let range = self.ranges.get(after.checked_sub(1)?)?;
        (pc < range.high).then_some(range)
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// A planted bug for the attribution's control (rule 53). `pub(crate)`: only the tests choose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The link register taken as the caller everywhere (D100 point 3 as first written): in a
    /// non-leaf body it names the function itself.
    #[cfg_attr(not(all(test, not(blaze_check))), expect(dead_code, reason = "the attribution's planted-bug control (sample/attribute_tests.rs)"))]
    LrAlways,
}

/// Where an entry's caller is read from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallerFrom {
    /// The link register: the prologue's first words, or a `ret`.
    Lr,
    /// The frame record's return address, `[x29 + 8]`.
    Frame,
}

/// Where `entry`'s caller is read from, its leaf being `leaf`.
pub const fn caller_from(entry: &SampleEntry, leaf: &Range) -> CallerFrom {
    caller_from_with(entry, leaf, Plant::None)
}

pub(crate) const fn caller_from_with(entry: &SampleEntry, leaf: &Range, plant: Plant) -> CallerFrom {
    if matches!(plant, Plant::LrAlways) {
        return CallerFrom::Lr;
    }
    // `pc` is inside `leaf` (the caller found it there), so the subtraction does not wrap.
    if entry.pc - leaf.low < PROLOGUE_BYTES || is_ret(entry.word) {
        CallerFrom::Lr
    } else {
        CallerFrom::Frame
    }
}

/// What one attribution saw, beside the profile: every entry is in exactly one of `attributed`
/// and `outside`, and every attributed one in exactly one of `edges`, `no_caller` and
/// `caller_outside`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Summary {
    /// Entries whose program counter is in a placed range.
    pub attributed: u64,
    /// Entries whose program counter is in no placed range (the launcher, a C library, a stub).
    pub outside: u64,
    /// Attributed entries that gave a call edge.
    pub edges: u64,
    /// Attributed entries with no address to read the caller from (the frame check refused, or
    /// the link register is 0).
    pub no_caller: u64,
    /// Attributed entries whose caller is in no placed range.
    pub caller_outside: u64,
}

/// The profile of `entries` against `placed`, in `alloc`, with what was seen.
pub fn attribute<'a>(
    entries: &[SampleEntry],
    placed: &Placed<'_>,
    alloc: Alloc<'a>,
) -> Result<(Profile<'a>, Summary), AttributeError> {
    attribute_with(entries, placed, alloc, Plant::None)
}

pub(crate) fn attribute_with<'a>(
    entries: &[SampleEntry],
    placed: &Placed<'_>,
    alloc: Alloc<'a>,
    plant: Plant,
) -> Result<(Profile<'a>, Summary), AttributeError> {
    let mut summary = Summary::default();
    let mut leaves: AVec<'a, Symbol> = Vec::with_capacity_in(entries.len(), alloc);
    let mut pairs: AVec<'a, (Symbol, Symbol)> = Vec::with_capacity_in(entries.len(), alloc);
    for entry in entries {
        let Some(leaf) = placed.find(entry.pc) else {
            summary.outside += 1;
            continue;
        };
        summary.attributed += 1;
        leaves.push(leaf.symbol);
        let ret = match caller_from_with(entry, leaf, plant) {
            CallerFrom::Lr => entry.lr,
            CallerFrom::Frame => entry.frame_ret,
        };
        if ret == 0 {
            summary.no_caller += 1;
            continue;
        }
        match placed.find(ret.wrapping_sub(WORD)) {
            Some(caller) => {
                summary.edges += 1;
                pairs.push((caller.symbol, leaf.symbol));
            }
            None => summary.caller_outside += 1,
        }
    }
    let mut profile = Profile::new_in(alloc);
    leaves.sort_unstable();
    for run in leaves.chunk_by(|a, b| a == b) {
        let samples = count(run.len(), run[0], None)?;
        profile.rows.push(ProfileRow { symbol: run[0], samples });
    }
    pairs.sort_unstable();
    for run in pairs.chunk_by(|a, b| a == b) {
        let (caller, callee) = run[0];
        let samples = count(run.len(), callee, Some(caller))?;
        profile.edges.push(EdgeRow { caller, callee, samples });
    }
    Ok((profile, summary))
}

/// A row's count as the record's `u32`, refused by name past it.
fn count(len: usize, symbol: Symbol, caller: Option<Symbol>) -> Result<u32, AttributeError> {
    u32::try_from(len).map_err(|_| AttributeError::TooMany { symbol, caller, samples: len as u64 })
}

/// The startup record as the parent's first-call machine sees calls come: each symbol's first
/// call, in order, until the program's entry has returned control ([`FirstCalls::close`]).
pub struct FirstCalls<'a> {
    order: AVec<'a, Symbol>,
    /// The symbols in `order`, sorted: the uniqueness check (the order itself is the record).
    seen: AVec<'a, Symbol>,
    closed: bool,
}

impl<'a> FirstCalls<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> FirstCalls<'a> {
        FirstCalls { order: Vec::new_in(alloc), seen: Vec::new_in(alloc), closed: false }
    }

    /// `symbol` was called for the first time in this run, as far as the caller knows; a symbol
    /// already recorded is nothing, and so is any call after [`FirstCalls::close`].
    pub fn called(&mut self, symbol: Symbol) {
        if self.closed {
            return;
        }
        if let Err(at) = self.seen.binary_search(&symbol) {
            self.seen.insert_shift(at, symbol);
            self.order.push(symbol);
        }
    }

    /// The program's entry returned control: later first calls are not startup.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// The record.
    pub fn record(self) -> Startup<'a> {
        Startup { order: self.order }
    }
}

/// Why attribution refused, naming the thing (rule 19); the design point is D100 point 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttributeError {
    /// A placed range of no bytes.
    EmptyRange { symbol: Symbol, low: u64, high: u64 },
    /// Two placed ranges overlap: `second` begins at `at`, inside `first`.
    Overlap { first: Symbol, second: Symbol, at: u64 },
    /// One row or edge holds more samples than its record's `u32`.
    TooMany { symbol: Symbol, caller: Option<Symbol>, samples: u64 },
}

impl fmt::Display for AttributeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AttributeError::EmptyRange { symbol, low, high } => write!(
                f,
                "attribution (D100 point 4): the placed range of {:02x?} is empty ({low:#x}..{high:#x})",
                &symbol.0[..8]
            ),
            AttributeError::Overlap { first, second, at } => write!(
                f,
                "attribution (D100 point 4): the placed range of {:02x?} begins at {at:#x}, inside that of {:02x?}",
                &second.0[..8],
                &first.0[..8]
            ),
            AttributeError::TooMany { symbol, caller: None, samples } => write!(
                f,
                "attribution (D100 point 4): {:02x?} has {samples} samples, past a profile row's u32",
                &symbol.0[..8]
            ),
            AttributeError::TooMany { symbol, caller: Some(caller), samples } => write!(
                f,
                "attribution (D100 point 4): the edge {:02x?} -> {:02x?} has {samples} samples, past a profile edge's u32",
                &caller.0[..8],
                &symbol.0[..8]
            ),
        }
    }
}

impl core::error::Error for AttributeError {}
