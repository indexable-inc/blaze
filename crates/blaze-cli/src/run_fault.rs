//! What `blaze run` says when its program died on a fault or a trap (rule 19: a crash names
//! the thing). The program's crash handler (blaze-launch-rt run_mode/fault_report.rs) writes
//! its own line, which names places by the first bytes of their ids (the child knows no paths),
//! and tells the parent the same places on the socket (`Up::Fault`, blaze-launch wire/fault.rs).
//! The parent knows each symbol's item and each record's bytes, so it says, once the program
//! has ended:
//!
//!     blaze run: it struck at +0x3fc in fn `std::rt::lang_start_internal` (symbol 662675a6bda2,
//!     record 2dd8929c5e48): `brk #0xff02`: an `unreachable` reached (code the compiler holds is
//!     never run), right after the call of fn `core::panicking::panic_fmt` (symbol 5a5a5a5a5a5a):
//!     that call returned, though the code after it is never to run
//!
//! (one line). The trap's reason is read from the instruction at the pc in the record's own
//! code (blaze-code trap.rs, the codes' one home), and WHICH construct it ends from the
//! record's trap table (blaze-code record.rs `TrapRow`: the pc's offset to the construct's
//! item-relative range and kind), its `file:line:col` made only here, for the message (rule
//! 71). The function's name is the walk's row for its symbol: the instance its name holds, by
//! the one printer of an item and with its ground arguments
//! ([`crate::run_parent::Lazy::name_of`]). The child's own line is written only when it could
//! not tell the parent (one report, rule 30).
//!
//! The line ends with the faulting thread's callers, as the child walked its frame-pointer
//! chain (run schema 3, wire/fault.rs `Callers`), the first eight each named by its symbol's
//! row as the pc is: `; called from fn `core::alloc::layout::Layout::array::inner` +0x54, from
//! fn `alloc::raw_vec::..` +0x1c0, from 0x1a2b3c, in no placed code, and 3 more (the walk
//! ended: the chain's end)`.
//!
//! Pure: [`report`] is given the names and the record's bytes; own_process.rs reads them.

use core::fmt;

use blaze_code::record::{CodeView, Held, Subject};
use blaze_code::trap::{self, Reason, TrapAt};
use blaze_launch::wire::fault::{Callers, Fault, NAMED_CALLERS, Placed, Regs, Spot};
use blaze_typeck::ItemKey;

use crate::run_parent::ParentError;

/// What names a symbol of the run.
#[derive(Debug)]
pub(crate) enum Named {
    /// The item by the one printer (front2 item_shown.rs `ItemShown`: its path, or for a
    /// path-less item WHERE it is written), and the instance's ground arguments as Rust spells
    /// them when it has any (`[u8, <closure 0 of core::fmt::..>]`).
    Shown { item: blaze_ladder::front2::item_shown::ItemShown, args: Option<Box<str>> },
    /// A path a front gave with no printer of its own (`Front::path_of`).
    Path(Box<str>),
    /// A row's item the names give neither a path nor a place for.
    Item(ItemKey),
    /// No row this run: a special record (the C entry shim, the thread-local getter or ender),
    /// or a row placed from today's start list and never stepped.
    NoRow,
}

/// Where a trap's construct is written, for the message (rule 71: made only here, from the trap
/// row's item-relative site).
#[derive(Debug)]
pub(crate) struct SiteShown {
    /// The file as the program observes it (`file!()`).
    pub file: Box<str>,
    pub line: u32,
    pub column: u32,
}

/// Why the trap at the pc was not read from its record.
#[derive(Debug)]
pub(crate) enum Unread {
    /// The record's file could not be read.
    File(ParentError),
    /// Its bytes are no code record.
    Decode(blaze_code::record::DecodeError),
}

/// The line, made while the walk is alive and said once the program has ended.
#[derive(Debug)]
pub(crate) struct Report {
    fault: Fault,
    at: Option<Named>,
    touched: Option<Named>,
    /// The trap at the pc, when the pc is in a function's code and the word there is a `brk`.
    trap: Option<TrapAt>,
    /// The function whose call the trap follows.
    callee: Option<Named>,
    /// Where the trap row's construct is written, when the row has a site `sites` placed.
    site: Option<SiteShown>,
    /// Why the pc's record was not read, when it was not.
    unread: Option<Unread>,
    /// The enum the trap row's subject names, when it has one.
    subject: Option<Named>,
    /// The callers the child named, each by its symbol's row (`None`: in no placed code).
    callers: [Option<Named>; NAMED_CALLERS],
}

/// What names a run's places for a report: the walk (run_parent.rs `Lazy`), while it is alive.
pub(crate) trait Places {
    /// The fn or static of code symbol `symbol`.
    fn named(&mut self, symbol: [u8; 32]) -> Named;
    /// Byte `at` of item `item`'s text (a trap row's item-relative site), placed in today's
    /// text; `None` when it cannot be.
    fn site(&mut self, item: ItemKey, at: u32) -> Option<SiteShown>;
    /// Item `item` itself (a trap row's subject: the enum whose discriminant was read).
    fn item(&mut self, item: ItemKey) -> Named;
}

/// The report of `fault`: each place named by `places`, the trap read from `record` (the bytes
/// of the record the pc is in: `None` when the pc is in no function's code), and its trap row's
/// construct placed by `places` too.
pub(crate) fn report(fault: Fault, places: &mut dyn Places, record: Option<Result<&[u8], ParentError>>) -> Report {
    let at = fault.at.map(|spot| places.named(spot.symbol.0));
    let touched = match (fault.touched, fault.at) {
        // The address is the pc's own place (a trap's `si_addr` is its pc): said once.
        (Some(touched), Some(at)) if touched == at => None,
        (Some(touched), _) => Some(places.named(touched.symbol.0)),
        (None, _) => None,
    };
    let mut trap = None;
    let mut unread = None;
    match (record, fault.at) {
        (Some(Ok(bytes)), Some(spot)) => match CodeView::read(bytes) {
            Ok(view) => trap = trap::at(&view, spot.offset),
            Err(why) => unread = Some(Unread::Decode(why)),
        },
        (Some(Err(why)), _) => unread = Some(Unread::File(why)),
        (Some(Ok(_)), None) | (None, _) => {}
    }
    let callee = trap.and_then(|trap| trap.after_call).map(|symbol| places.named(symbol.0));
    let site = trap.and_then(|trap| trap.construct).and_then(|row| row.site).and_then(|(item, at)| places.site(ItemKey(item), at));
    let subject = trap.and_then(|trap| trap.construct).and_then(|row| row.subject).map(|subject| places.item(ItemKey(subject.item)));
    let mut callers = [const { None }; NAMED_CALLERS];
    if let Some(walked) = fault.callers {
        for (named, spot) in callers.iter_mut().zip(walked.spots()) {
            *named = spot.map(|spot| places.named(spot.symbol.0));
        }
    }
    Report { fault, at, touched, trap, callee, site, unread, subject, callers }
}

/// `; called from <each named caller> +<offset>, .., and N more (the walk ended: <why>)`: the
/// callers the child walked, innermost first; nothing when this host's child walks none.
fn callers_said(f: &mut fmt::Formatter<'_>, walked: &Callers, named: &[Option<Named>; NAMED_CALLERS]) -> fmt::Result {
    if walked.rets().is_empty() {
        return write!(f, "; no caller was walked (the walk ended: {})", walked.end.says());
    }
    f.write_str("; called from ")?;
    for (i, (&ret, spot)) in walked.rets().iter().zip(walked.spots()).enumerate() {
        if i > 0 {
            f.write_str(", from ")?;
        }
        match (spot, &named[i]) {
            (Some(spot), Some(named)) => write!(f, "{} +{:#x}", Thing(named, "fn"), spot.offset)?,
            _ => write!(f, "{ret:#x}, in no placed code (the launcher's own, or a stub)")?,
        }
    }
    let more = walked.rets().len() - walked.spots().len();
    if more > 0 {
        write!(f, ", and {more} more")?;
    }
    write!(f, " (the walk ended: {})", walked.end.says())
}

/// What a trap row says the switched value is, read from the fault's registers (the child's,
/// at the `brk`): ` it read 0x3 (x9)`; ` it read a value the code holds in x9, and this host
/// sent no registers`; nothing when the row says the value is not known.
fn held_value(f: &mut fmt::Formatter<'_>, held: Held, regs: Option<&Regs>) -> fmt::Result {
    match held {
        Held::Unknown => Ok(()),
        Held::Reg(n) => match regs.and_then(|regs| regs.general(n)) {
            Some(value) => write!(f, ": it read {value:#x} (x{n})"),
            None => write!(f, ": it read the value in x{n}, and the program sent no registers on this host"),
        },
        // A slot's value is in the child's stack; it sends the registers alone.
        Held::Stack(offset) => match regs {
            Some(regs) => write!(f, ": it read the 8 bytes at sp + {offset:#x} (sp {:#x})", regs.sp()),
            None => write!(f, ": it read the 8 bytes at sp + {offset:#x}"),
        },
    }
}

/// `; `AlignmentEnum` has discriminants 0x1, 0x2, 0x4, .. (64)`: the subject and the
/// discriminants its row keeps, `..` and the count when it has more.
fn subject_said(f: &mut fmt::Formatter<'_>, subject: &Subject, named: &Named) -> fmt::Result {
    match named {
        // Its last segment: the fn's path is said already; the enum's own name says enough.
        Named::Shown { item, .. } => match item.path() {
            Some(path) => write!(f, "; `{}` has discriminants ", path.rsplit("::").next().unwrap_or(path))?,
            None => write!(f, "; {} has discriminants ", Thing(named, "enum"))?,
        },
        Named::Path(path) => write!(f, "; `{}` has discriminants ", path.rsplit("::").next().unwrap_or(path))?,
        other => write!(f, "; {} has discriminants ", Thing(other, "enum"))?,
    }
    for (i, d) in subject.shown().iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{d:#x}")?;
    }
    match subject.count as usize > subject.shown().len() {
        true => write!(f, ", .. ({})", subject.count),
        false => Ok(()),
    }
}

/// The first six bytes in hex.
struct Short<'b>(&'b [u8; 32]);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// A key's 16 bytes in hex, `0x` first.
struct Hex16<'b>(&'b [u8; 16]);

impl fmt::Display for Hex16<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// `kind `name``, or what stands for a name.
struct Thing<'n>(&'n Named, &'static str);

impl fmt::Display for Thing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Thing(named, kind) = self;
        match *named {
            Named::Shown { item, args } => {
                match (item.path(), item.written()) {
                    (Some(path), _) => write!(f, "{kind} `{path}`")?,
                    (None, Some(written)) => write!(f, "{kind} with no path, {written} (key {})", Hex16(&item.key()))?,
                    (None, None) => write!(f, "{kind} of item {} (the names give it neither a path nor a place)", Hex16(&item.key()))?,
                }
                match args {
                    Some(args) => write!(f, " at {args}"),
                    None => Ok(()),
                }
            }
            Named::Path(path) => write!(f, "{kind} `{path}`"),
            Named::Item(item) => write!(f, "{kind} of item {:#x} (the names give it neither a path nor a place)", item.0),
            Named::NoRow => write!(f, "a {kind} this run has no row for (a special record: the C entry shim, the thread-local getter or ender; or a row placed from the start list)"),
        }
    }
}

fn place(f: &mut fmt::Formatter<'_>, spot: &Spot, named: &Named) -> fmt::Result {
    let (part, kind) = match spot.placed {
        Placed::Code => ("", "fn"),
        Placed::Relro => (" the relro words of", "fn"),
        Placed::ReadOnlyDatum => (" the read-only datum of", "static"),
        Placed::WritableDatum => (" the writable datum of", "static"),
    };
    write!(f, "+{:#x} in{part} {} (symbol {}, record {})", spot.offset, Thing(named, kind), Short(&spot.symbol.0), Short(&spot.record.0))
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze run: the program was killed by {}: ", self.fault.signal.name())?;
        match (&self.fault.at, &self.at) {
            (Some(spot), Some(named)) => {
                f.write_str("it struck at ")?;
                place(f, spot, named)?;
            }
            _ => match self.fault.pc {
                Some(pc) => write!(f, "its pc {pc:#x} is in no placed function")?,
                None => f.write_str("its pc is not read on this host")?,
            },
        }
        if let Some(trap) = self.trap {
            write!(f, ": `brk #{:#x}`: {}", trap.reason.code(), trap.reason.says())?;
            // WHICH construct, from the record's trap table: only what the row says.
            if let Some(row) = trap.construct {
                write!(f, ": {}", row.kind.construct())?;
                match (&self.site, row.site) {
                    (Some(site), _) => write!(f, " at {}:{}:{}", site.file, site.line, site.column)?,
                    (None, Some((item, at))) => write!(f, " at byte {at} of item {} (not placed in today's text)", Hex16(&item.to_be_bytes()))?,
                    // Said, never silent (piece 3): the build made the construct outside any
                    // task (a parameter's copy before the body's first), or its expression has
                    // no range in its item's text.
                    (None, None) => f.write_str(" (the build kept no site: it was made outside any of the body's expressions, or that expression has no range in its item's text)")?,
                }
                write!(f, " {}", row.kind.happened())?;
                held_value(f, row.held, self.fault.regs.as_ref())?;
                if let (Some(subject), Some(named)) = (row.subject, &self.subject) {
                    subject_said(f, &subject, named)?;
                }
            }
            if let Some(callee) = &self.callee {
                write!(f, ", right after the call of {}", Thing(callee, "fn"))?;
                if let Some(symbol) = trap.after_call {
                    write!(f, " (symbol {})", Short(&symbol.0))?;
                }
                match trap.reason {
                    Reason::Unreachable => f.write_str(": that call returned, though the code after it is never to run (a fn typed `-> !` returned, or its stub does)")?,
                    _ => f.write_str(": that call returned into the trap")?,
                }
            }
        }
        if let Some(unread) = &self.unread {
            match unread {
                Unread::File(why) => write!(f, " (its record was not read: {why})")?,
                Unread::Decode(why) => write!(f, " (its record was not read: {why})")?,
            }
        }
        if let (Some(spot), Some(named)) = (&self.fault.touched, &self.touched) {
            let access = match self.fault.write {
                Some(true) => "it wrote",
                Some(false) => "it read",
                None => "the address",
            };
            write!(f, "; {access} {:#x}, ", self.fault.address)?;
            place(f, spot, named)?;
        }
        if let Some(walked) = &self.fault.callers {
            callers_said(f, walked, &self.callers)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "run_fault_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run_fault_trap_tests.rs"]
mod trap_tests;

#[cfg(test)]
#[path = "run_fault_held_tests.rs"]
mod held_tests;

#[cfg(test)]
#[path = "run_fault_callers_tests.rs"]
mod callers_tests;
