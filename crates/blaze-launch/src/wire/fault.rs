//! The run wire's [`super::run::Up::Fault`]: a program that dies on a fault or a trap tells its
//! parent where it struck, as WHAT IS PLACED THERE, before it dies (rule 19: the parent knows
//! each symbol's item and each record's bytes, the child knows neither paths nor reasons). The
//! child's crash handler (blaze-launch-rt run_mode/fault_report.rs) writes it, async-signal-
//! safely, onto the socket with the message id 0 (it is no ask; nothing answers it); the parent
//! keeps the first one it reads and, when the program has ended by a signal, says the line
//! naming the function by its path and the trap by its reason (blaze-cli run_fault.rs).
//!
//! The body is `{"Fault": {"at": Spot|null, "pc": uint|null, "code": Around|null, "regs":
//! Regs|null, "write": bool|null, "signal": text, "address": uint, "touched": Spot|null}}`; a
//! spot is `{"offset": uint, "placed": text, "record": bytes32, "symbol": bytes32}`; the code
//! around the pc is `{"bytes": bytes (at most 32), "start": uint}`; the registers are one
//! architecture's variant, `{"Arm64": {"x": [31 uints], "pc": uint, "sp": uint, "pstate":
//! uint}}` or `{"X86_64": {"gpr": [16 uints], "rip": uint, "rflags": uint}}`: every map's keys
//! in the order of their encodings (by length, then by bytes). Run schema 2 added `code` and
//! `regs` (run.rs `RUN_SCHEMA`).

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

/// The general registers of an AArch64 thread at the fault: `x0..x30`, `sp`, `pc`, `pstate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Arm64 {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
}

/// The general registers of an x86-64 thread at the fault: the sixteen in their encoding order
/// (`rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15`), `rip`, `rflags`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct X86_64 {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
}

/// The thread's general registers at the fault, in its architecture's own form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Regs {
    Arm64(Arm64),
    X86_64(X86_64),
}

impl Regs {
    /// General register `n` (AArch64 `xN`, `n` up to 30; x86-64 in the encoding order, `n` up
    /// to 15); `None` past them.
    pub fn general(&self, n: u8) -> Option<u64> {
        match self {
            Regs::Arm64(a) => a.x.get(usize::from(n)).copied(),
            Regs::X86_64(x) => x.gpr.get(usize::from(n)).copied(),
        }
    }

    /// The stack pointer.
    pub fn sp(&self) -> u64 {
        match self {
            Regs::Arm64(a) => a.sp,
            Regs::X86_64(x) => x.gpr[4],
        }
    }
}

/// The most code bytes [`Around`] carries: 16 before the pc and 16 from it.
pub const AROUND: usize = 32;

/// The code bytes around the pc that the child could read: `len` bytes from run-view address
/// `start` (inside the placed code the pc is in, so at most [`AROUND`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Around {
    pub start: u64,
    pub len: u8,
    pub bytes: [u8; AROUND],
}

impl Around {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len).min(AROUND)]
    }
}

/// Where a program struck: the signal, the pc (`None` where it is not read) and what is placed
/// there, the code around the pc and the thread's registers (`None` where they are not read),
/// the faulting address (`si_addr`) and what is placed there, and whether the access was a
/// write (`None` where it is not known).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault {
    pub signal: Signal,
    pub pc: Option<u64>,
    pub at: Option<Spot>,
    pub code: Option<Around>,
    pub regs: Option<Regs>,
    pub address: u64,
    pub touched: Option<Spot>,
    pub write: Option<bool>,
}

fn write_around<A: Allocator>(to: &mut Writer<'_, A>, around: Option<Around>) {
    let Some(around) = around else {
        to.null();
        return;
    };
    to.map(2);
    to.text("bytes");
    to.bytes(around.bytes());
    to.text("start");
    to.uint(around.start);
}

fn read_around(from: &mut Reader<'_>) -> Result<Option<Around>, WireError> {
    if from.null() {
        return Ok(None);
    }
    from.map(2, "the code around the pc, a map of 2")?;
    from.key("bytes")?;
    let at = from.at();
    let read = from.bytes()?;
    if read.len() > AROUND {
        return Err(WireError::Range { at, what: "the code around the pc is at most 32 bytes" });
    }
    let mut bytes = [0u8; AROUND];
    bytes[..read.len()].copy_from_slice(read);
    from.key("start")?;
    let start = from.uint()?;
    Ok(Some(Around { start, len: read.len() as u8, bytes }))
}

fn write_words<A: Allocator>(to: &mut Writer<'_, A>, words: &[u64]) {
    to.array(words.len());
    for &word in words {
        to.uint(word);
    }
}

fn read_words<const N: usize>(from: &mut Reader<'_>, what: &'static str) -> Result<[u64; N], WireError> {
    let at = from.at();
    if from.array()? != N {
        return Err(WireError::Range { at, what });
    }
    let mut words = [0u64; N];
    for word in words.iter_mut() {
        *word = from.uint()?;
    }
    Ok(words)
}

fn write_regs<A: Allocator>(to: &mut Writer<'_, A>, regs: Option<Regs>) {
    match regs {
        None => to.null(),
        Some(Regs::Arm64(a)) => {
            variant(to, "Arm64", 4);
            to.text("x");
            write_words(to, &a.x);
            to.text("pc");
            to.uint(a.pc);
            to.text("sp");
            to.uint(a.sp);
            to.text("pstate");
            to.uint(a.pstate);
        }
        Some(Regs::X86_64(x)) => {
            variant(to, "X86_64", 3);
            to.text("gpr");
            write_words(to, &x.gpr);
            to.text("rip");
            to.uint(x.rip);
            to.text("rflags");
            to.uint(x.rflags);
        }
    }
}

fn read_regs(from: &mut Reader<'_>) -> Result<Option<Regs>, WireError> {
    if from.null() {
        return Ok(None);
    }
    from.map(1, "the registers, a map of 1 (the architecture)")?;
    let named = from.at();
    match from.text()? {
        "Arm64" => {
            from.map(4, "Arm64, a map of 4")?;
            from.key("x")?;
            let x = read_words::<31>(from, "x0..x30 are 31 words")?;
            from.key("pc")?;
            let pc = from.uint()?;
            from.key("sp")?;
            let sp = from.uint()?;
            from.key("pstate")?;
            let pstate = from.uint()?;
            Ok(Some(Regs::Arm64(Arm64 { x, sp, pc, pstate })))
        }
        "X86_64" => {
            from.map(3, "X86_64, a map of 3")?;
            from.key("gpr")?;
            let gpr = read_words::<16>(from, "the general registers are 16 words")?;
            from.key("rip")?;
            let rip = from.uint()?;
            from.key("rflags")?;
            let rflags = from.uint()?;
            Ok(Some(Regs::X86_64(X86_64 { gpr, rip, rflags })))
        }
        _ => Err(WireError::Variant { at: named }),
    }
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
    variant(to, NAME, 8);
    to.text("at");
    write_spot(to, fault.at);
    to.text("pc");
    match fault.pc {
        Some(pc) => to.uint(pc),
        None => to.null(),
    }
    to.text("code");
    write_around(to, fault.code);
    to.text("regs");
    write_regs(to, fault.regs);
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
    from.map(8, "Fault, a map of 8")?;
    from.key("at")?;
    let at = read_spot(from)?;
    from.key("pc")?;
    let pc = match from.null() {
        true => None,
        false => Some(from.uint()?),
    };
    from.key("code")?;
    let code = read_around(from)?;
    from.key("regs")?;
    let regs = read_regs(from)?;
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
    Ok(Fault { signal, pc, at, code, regs, address, touched, write })
}
