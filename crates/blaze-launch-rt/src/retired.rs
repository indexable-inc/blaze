//! Retired code that must never run again: the ranges whose pages the launcher gave back
//! ([`blaze_launch::Write::Decommit`]) and made inaccessible, so that a late call through a
//! stale address traps instead of running whatever came after it. The area never hands an
//! offset out twice, so a range stays retired for the life of the process and its row is never
//! removed: the table only grows, and a fault handler can read it without a lock.
//!
//! - **The launcher thread** appends a row per body it gives back, through the one [`Appender`]
//!   ([`Table::appender`]), *before* the pages become inaccessible: a fault in them always finds
//!   its row.
//! - **A fault handler** looks the faulting address up ([`Table::which`]) and, when it is
//!   retired code, writes the stop line into a buffer of its own ([`report`]) and writes that to
//!   stderr itself. Both are async-signal-safe: atomic loads, no allocation, no lock, no call
//!   into anything that could hold one. Which signal, which stack and what follows (the stop) are
//!   the handler's (sys.rs).
//!
//! The orderings. The appender writes the four fields of row `n` with relaxed stores, then
//! publishes the row by storing `n + 1` to `published` with `Release`. A reader loads
//! `published` with `Acquire` and reads only rows below it: the acquire load that sees `n + 1`
//! synchronizes with the release store, so all four field stores happen before the reader's
//! loads and it sees the whole row, never half of one. A row is written once and never again
//! (the table is append-only and rows below `published` are never touched), so no later store
//! can tear it either. Single writer: the appender is the only way to write and there is one of
//! it ([`Table::appender`] refuses a second), so `published` needs no read-modify-write.
//!
//! Adapted from makepad/relax (MIT), compiler/src/invalid.rs and compiler/src/stale.rs: code
//! made `PROT_NONE` and never reused; an append-only table read lock-free by the fault handler.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use blaze_launch::Slot;

/// One retired range: the code of one version of one function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Retired {
    /// The first byte, as an address of this process.
    pub start: usize,
    /// How many bytes, at least one.
    pub len: usize,
    /// The function whose code it was: its slot.
    pub function: Slot,
    /// Which version of the function: the count of bodies bound to the slot before this one.
    pub version: u64,
}

impl Retired {
    /// One past the last byte. [`Appender::append`] refused a range for which this wraps.
    pub fn end(&self) -> usize {
        self.start + self.len
    }

    fn holds(&self, address: usize) -> bool {
        self.start <= address && address < self.end()
    }
}

/// Why a row was not appended (rule 19: each names the range and what refused it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppendError {
    /// The table holds `capacity` rows already: `range` was not recorded, so its pages must not
    /// be made inaccessible either (a fault there would find no row). The capacity is too small
    /// for this run.
    Full { capacity: usize, range: Retired },
    /// `range` has no bytes, or its end wraps the address space.
    Empty { range: Retired },
    /// `range` overlaps a range retired before: the area handed an offset out twice, which it
    /// never does, so this is a blaze bug in the launcher.
    Overlaps { range: Retired, earlier: Retired },
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { capacity, range } => write!(
                f,
                "the retired-code table is full ({capacity} rows): cannot retire {range:x?}"
            ),
            Self::Empty { range } => write!(f, "retired range {range:x?} is empty or wraps"),
            Self::Overlaps { range, earlier } => write!(
                f,
                "retired range {range:x?} overlaps {earlier:x?}: an offset was handed out twice"
            ),
        }
    }
}

impl core::error::Error for AppendError {}

/// The second [`Table::appender`]: there is one writer of a table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppenderTaken;

impl fmt::Display for AppenderTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the retired-code table already has its appender (one writer, the launcher)")
    }
}

impl core::error::Error for AppenderTaken {}

/// One row, every field an atomic so a reader on another thread or in a signal handler may load
/// it while the appender writes later rows.
struct Row {
    start: AtomicUsize,
    len: AtomicUsize,
    function: AtomicU32,
    version: AtomicU64,
}

impl Row {
    const EMPTY: Self = Self {
        start: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
        function: AtomicU32::new(0),
        version: AtomicU64::new(0),
    };

    /// Reads a published row. Relaxed is enough: the caller's `Acquire` load of `published`
    /// ordered every store of this row before these loads (the module's orderings).
    fn load(&self) -> Retired {
        Retired {
            start: self.start.load(Ordering::Relaxed),
            len: self.len.load(Ordering::Relaxed),
            function: Slot(self.function.load(Ordering::Relaxed)),
            version: self.version.load(Ordering::Relaxed),
        }
    }
}

/// The retired ranges of one process: at most `N` rows, kept in place (a `static`), never
/// allocated.
pub struct Table<const N: usize> {
    rows: [Row; N],
    /// How many rows are whole: the appender's `Release` store, a reader's `Acquire` load.
    published: AtomicUsize,
    /// Whether the one [`Appender`] was handed out.
    taken: AtomicBool,
}

impl<const N: usize> Table<N> {
    /// An empty table, for a `static`.
    pub const fn new() -> Self {
        Self { rows: [Row::EMPTY; N], published: AtomicUsize::new(0), taken: AtomicBool::new(false) }
    }

    /// The one writer of this table. A second call is refused: two writers would race on
    /// `published`.
    pub fn appender(&self) -> Result<Appender<'_, N>, AppenderTaken> {
        // AcqRel: the swap is the claim; it orders nothing else, a winner is all it decides.
        if self.taken.swap(true, Ordering::AcqRel) {
            return Err(AppenderTaken);
        }
        Ok(Appender { table: self })
    }

    /// The retired range that holds `address`, if any. Async-signal-safe: atomic loads only.
    /// Linear in the rows: it runs once, on the way to a stop.
    pub fn which(&self, address: usize) -> Option<Retired> {
        let published = self.published.load(Ordering::Acquire);
        self.rows[..published].iter().map(Row::load).find(|it| it.holds(address))
    }

    /// How many ranges are retired.
    pub fn len(&self) -> usize {
        self.published.load(Ordering::Acquire)
    }

    /// Whether no range is retired yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<const N: usize> Default for Table<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The one writer of a [`Table`]: the launcher thread's. Not `Clone`, and `append` takes
/// `&mut self`, so at most one append runs at a time.
pub struct Appender<'t, const N: usize> {
    table: &'t Table<N>,
}

impl<const N: usize> Appender<'_, N> {
    /// Records `range` as retired. Call it before the range's pages become inaccessible.
    pub fn append(&mut self, range: Retired) -> Result<(), AppendError> {
        if range.len == 0 || range.start.checked_add(range.len).is_none() {
            return Err(AppendError::Empty { range });
        }
        // Only this appender stores `published`, so a relaxed load reads its own last store.
        let n = self.table.published.load(Ordering::Relaxed);
        let earlier = self.table.rows[..n].iter().map(Row::load).find(|it| {
            it.start < range.end() && range.start < it.end()
        });
        if let Some(earlier) = earlier {
            return Err(AppendError::Overlaps { range, earlier });
        }
        let Some(row) = self.table.rows.get(n) else {
            return Err(AppendError::Full { capacity: N, range });
        };
        // No reader looks at row `n` before `published` says so, so these stores race nothing.
        row.start.store(range.start, Ordering::Relaxed);
        row.len.store(range.len, Ordering::Relaxed);
        row.function.store(range.function.0, Ordering::Relaxed);
        row.version.store(range.version, Ordering::Relaxed);
        // Release: publishes the four stores above to every reader that acquires `n + 1`.
        self.table.published.store(n + 1, Ordering::Release);
        Ok(())
    }
}

/// The buffer given to [`report`] was too short: `needed` bytes would have fit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReportTooLong {
    pub needed: usize,
    pub had: usize,
}

impl fmt::Display for ReportTooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the retired-code report needs {} bytes, the buffer has {}", self.needed, self.had)
    }
}

impl core::error::Error for ReportTooLong {}

/// A buffer of [`REPORT_MAX`] bytes always holds a [`report`].
pub const REPORT_MAX: usize = 320;

/// What to attach, the report's second line.
const ATTACH: &[u8] = b"attach to the report: these two lines, the output of `blaze --version`, \
the run's profile, and the edit made just before the stop\n";

/// Writes the stop line for a call that reached `retired` at `address` into `out`, and gives
/// how many bytes it wrote. Async-signal-safe: no allocation, no lock, no formatting machinery
/// (`core::fmt` may be fine, but nothing promises it); only byte copies into `out`.
///
/// The bytes, for function 7 version 2 at `0x1000`:
///
/// ```text
/// stopped: a call reached retired code of function 7 version 2 at 0x0000000000001000; this is a blaze bug
/// attach to the report: these two lines, the output of `blaze --version`, the run's profile, and the edit made just before the stop
/// ```
pub fn report(retired: &Retired, address: usize, out: &mut [u8]) -> Result<usize, ReportTooLong> {
    let mut w = Cursor { out, at: 0, needed: 0 };
    w.bytes(b"stopped: a call reached retired code of function ");
    w.decimal(u64::from(retired.function.0));
    w.bytes(b" version ");
    w.decimal(retired.version);
    w.bytes(b" at ");
    w.hex(address);
    w.bytes(b"; this is a blaze bug\n");
    w.bytes(ATTACH);
    if w.needed > w.out.len() {
        return Err(ReportTooLong { needed: w.needed, had: w.out.len() });
    }
    Ok(w.at)
}

/// Bytes written into a fixed buffer; what does not fit is counted, not written.
struct Cursor<'b> {
    out: &'b mut [u8],
    at: usize,
    needed: usize,
}

impl Cursor<'_> {
    fn bytes(&mut self, bytes: &[u8]) {
        self.needed += bytes.len();
        if let Some(to) = self.out.get_mut(self.at..self.at + bytes.len()) {
            to.copy_from_slice(bytes);
            self.at += bytes.len();
        } else {
            // Nothing more is written once one piece did not fit, so a short buffer never holds a
            // line with a hole in it; `report` refuses by `needed`.
            self.at = self.out.len();
        }
    }

    fn decimal(&mut self, mut n: u64) {
        let mut digits = [0u8; 20];
        let mut first = digits.len();
        loop {
            first -= 1;
            digits[first] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.bytes(&digits[first..]);
    }

    /// `0x` and every nibble of the address, so the line has one width for every address.
    fn hex(&mut self, n: usize) {
        const NIBBLES: usize = 2 * size_of::<usize>();
        let mut text = [0u8; 2 + NIBBLES];
        text[0] = b'0';
        text[1] = b'x';
        for (i, to) in text[2..].iter_mut().enumerate() {
            let nibble = (n >> (4 * (NIBBLES - 1 - i))) & 0xf;
            *to = b"0123456789abcdef"[nibble];
        }
        self.bytes(&text);
    }
}

#[cfg(test)]
mod tests;
