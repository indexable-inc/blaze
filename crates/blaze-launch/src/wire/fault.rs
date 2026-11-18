//! The run wire's [`super::run::Up::Fault`]: a program that dies on a fault or a trap tells its
//! parent where it struck, as WHAT IS PLACED THERE, before it dies (rule 19: the parent knows
//! each symbol's item and each record's bytes, the child knows neither paths nor reasons). The
//! child's crash handler (blaze-launch-rt run_mode/fault_report.rs) writes it, async-signal-
//! safely, onto the socket with the message id 0 (it is no ask; nothing answers it); the parent
//! keeps the first one it reads and, when the program has ended by a signal, says the line
//! naming the function by its path and the trap by its reason (blaze-cli run_fault.rs).
//!
//! The body is `{"Fault": {"at": Spot|null, "pc": uint|null, "write": bool|null, "signal":
//! text, "address": uint, "touched": Spot|null}}` and a spot is `{"offset": uint, "placed":
//! text, "record": bytes32, "symbol": bytes32}`, every map's keys in the order of their
//! encodings (by length, then by bytes).

use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::run::Symbol;
use super::{RecordId, WireError, id, variant};

/// The variant's name in a body.
pub(super) const NAME: &str = "Fault";

/// The signal the program dies by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    Segv,
    Bus,
    Ill,
    Trap,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Signal::Segv => "SIGSEGV",
            Signal::Bus => "SIGBUS",
            Signal::Ill => "SIGILL",
            Signal::Trap => "SIGTRAP",
        }
    }

    fn of(name: &str) -> Option<Signal> {
        [Signal::Segv, Signal::Bus, Signal::Ill, Signal::Trap].into_iter().find(|signal| signal.name() == name)
    }
}

/// What kind of placement an address is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placed {
    /// A function's code.
    Code,
    /// A function's relro words.
    Relro,
    /// A static's (vtable's, thread-local template's) read-only datum.
    ReadOnlyDatum,
    /// A static's datum in the writable data area.
    WritableDatum,
}

impl Placed {
    pub fn name(self) -> &'static str {
        match self {
            Placed::Code => "Code",
            Placed::Relro => "Relro",
            Placed::ReadOnlyDatum => "ReadOnlyDatum",
            Placed::WritableDatum => "WritableDatum",
        }
    }

    fn of(name: &str) -> Option<Placed> {
        [Placed::Code, Placed::Relro, Placed::ReadOnlyDatum, Placed::WritableDatum].into_iter().find(|placed| placed.name() == name)
    }
}

/// An address as what is placed there: `offset` bytes into the `placed` part of `symbol`'s
/// record `record`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spot {
    pub placed: Placed,
    pub symbol: Symbol,
    pub record: RecordId,
    pub offset: u64,
}

/// Where a program struck: the signal, the pc (`None` where it is not read) and what is placed
/// there, the faulting address (`si_addr`) and what is placed there, and whether the access was
/// a write (`None` where it is not known).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault {
    pub signal: Signal,
    pub pc: Option<u64>,
    pub at: Option<Spot>,
    pub address: u64,
    pub touched: Option<Spot>,
    pub write: Option<bool>,
}

fn write_spot<A: Allocator>(to: &mut Writer<'_, A>, spot: Option<Spot>) {
    let Some(spot) = spot else {
        to.null();
        return;
    };
    to.map(4);
    to.text("offset");
    to.uint(spot.offset);
    to.text("placed");
    to.text(spot.placed.name());
    to.text("record");
    to.bytes(&spot.record.0);
    to.text("symbol");
    to.bytes(&spot.symbol.0);
}

fn read_spot(from: &mut Reader<'_>) -> Result<Option<Spot>, WireError> {
    if from.null() {
        return Ok(None);
    }
    from.map(4, "a spot, a map of 4")?;
    from.key("offset")?;
    let offset = from.uint()?;
    from.key("placed")?;
    let at = from.at();
    let placed = Placed::of(from.text()?).ok_or(WireError::Variant { at })?;
    from.key("record")?;
    let record = RecordId(id(from, "a record id is 32 bytes")?);
    from.key("symbol")?;
    let symbol = Symbol(id(from, "a symbol is 32 bytes")?);
    Ok(Some(Spot { placed, symbol, record, offset }))
}

/// Writes the variant's body.
pub(super) fn write<A: Allocator>(to: &mut Writer<'_, A>, fault: &Fault) {
    variant(to, NAME, 6);
    to.text("at");
    write_spot(to, fault.at);
    to.text("pc");
    match fault.pc {
        Some(pc) => to.uint(pc),
        None => to.null(),
    }
    to.text("write");
    match fault.write {
        Some(write) => to.bool(write),
        None => to.null(),
    }
    to.text("signal");
    to.text(fault.signal.name());
    to.text("address");
    to.uint(fault.address);
    to.text("touched");
    write_spot(to, fault.touched);
}

/// Reads the variant's fields, the reader just past its name.
pub(super) fn read(from: &mut Reader<'_>) -> Result<Fault, WireError> {
    from.map(6, "Fault, a map of 6")?;
    from.key("at")?;
    let at = read_spot(from)?;
    from.key("pc")?;
    let pc = match from.null() {
        true => None,
        false => Some(from.uint()?),
    };
    from.key("write")?;
    let write = match from.null() {
        true => None,
        false => Some(from.bool()?),
    };
    from.key("signal")?;
    let named = from.at();
    let signal = Signal::of(from.text()?).ok_or(WireError::Variant { at: named })?;
    from.key("address")?;
    let address = from.uint()?;
    from.key("touched")?;
    let touched = read_spot(from)?;
    Ok(Fault { signal, pc, at, address, touched, write })
}
