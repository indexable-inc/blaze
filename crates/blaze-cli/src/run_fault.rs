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
//! code (blaze-code trap.rs, the codes' one home): no side table, so the record's bytes and its
//! key's meaning are unchanged. The function's name is the walk's row for its symbol (the
//! instance's tabled item) and the names records' path for that item
//! ([`crate::run_parent::Lazy::name_of`]).
//!
//! Pure: [`report`] is given the names and the record's bytes; own_process.rs reads them.

use core::fmt;

use blaze_code::record::CodeView;
use blaze_code::trap::{self, Reason, TrapAt};
use blaze_launch::wire::fault::{Fault, Placed, Spot};
use blaze_typeck::ItemKey;

use crate::run_parent::ParentError;

/// What names a symbol of the run.
#[derive(Debug)]
pub(crate) enum Named {
    /// The item's path, from the names records.
    Path(Box<str>),
    /// A row's item whose path the names records do not give.
    Item(ItemKey),
    /// No row this run: a special record (the C entry shim, the thread-local getter or ender),
    /// or a row placed from today's start list and never stepped.
    NoRow,
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
    /// Why the pc's record was not read, when it was not.
    unread: Option<Unread>,
}

/// The report of `fault`: each place named by `names`, the trap read from `record` (the bytes
/// of the record the pc is in: `None` when the pc is in no function's code).
pub(crate) fn report(fault: Fault, names: &mut dyn FnMut([u8; 32]) -> Named, record: Option<Result<&[u8], ParentError>>) -> Report {
    let at = fault.at.map(|spot| names(spot.symbol.0));
    let touched = match (fault.touched, fault.at) {
        // The address is the pc's own place (a trap's `si_addr` is its pc): said once.
        (Some(touched), Some(at)) if touched == at => None,
        (Some(touched), _) => Some(names(touched.symbol.0)),
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
    let callee = trap.and_then(|trap| trap.after_call).map(|symbol| names(symbol.0));
    Report { fault, at, touched, trap, callee, unread }
}

/// The first six bytes in hex.
struct Short<'b>(&'b [u8; 32]);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// `kind `name``, or what stands for a name.
struct Thing<'n>(&'n Named, &'static str);

impl fmt::Display for Thing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Thing(named, kind) = self;
        match *named {
            Named::Path(path) => write!(f, "{kind} `{path}`"),
            Named::Item(item) => write!(f, "{kind} of item {:#x} (the names records give no path)", item.0),
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
        Ok(())
    }
}

#[cfg(test)]
#[path = "run_fault_tests.rs"]
mod tests;
