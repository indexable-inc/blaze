//! The run path's wire (run.md, "The live picture"): what a program started by `blaze run` and
//! its parent say over the socket pair on descriptor 3. Not the patch protocol of the module
//! above ([`super::Ask`], [`super::Tell`], the daemon's): its own versioned schema
//! ([`RUN_SCHEMA`]), in the same core deterministic encoding, framed the same way (a four-byte
//! big-endian length, then one item), read with [`super::split`].
//!
//! The envelope is `{id, run, body}`: the key `run` holds the run wire's schema, so a patch
//! protocol message (`{id, body, schema}`) is refused at its second key by name, and a run
//! message of another schema is refused as [`WireError::RunSchema`] before a byte of its body
//! is read: an old peer is refused, never misread.
//!
//! A code record names its targets by SYMBOL (blaze-code `record::Target`), never by slot. The
//! parent numbers slots ([`Down::Declare`]): before it sends a record whose targets the program
//! has no slot for, it tells the program each new (symbol, slot) pair, so placing a record
//! never meets a symbol without a slot. A need names a slot, or a symbol the program has no
//! slot for yet ([`Needed`]); a symbol the parent cannot resolve is answered
//! [`Down::Failed`] with [`Failure::Unresolved`], naming it.
//!
//! What this schema has now is piece 4 of run.md's "Pieces": the need, the declared pairs, the
//! publish and the failure. The welcome, the start, hooks, heat and the exit come with pieces 5
//! and 6, each a step of [`RUN_SCHEMA`].
//!
//! Nothing here allocates on its own or does IO: a message is written onto the end of the
//! caller's buffer and read in place, borrowing its lists and text from the frame.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::cbor::{Reader, Writer};
use super::{LONGEST_FRAME, List, Listed, RecordId, WireError, data, id, native, personality, thread_locals, tier, tier_name, variant};
use crate::{Slot, Thread, Tier};

/// The version of the run wire's bodies. A reader of another refuses the message by name
/// ([`WireError::RunSchema`]). 2: `Up::Fault` carries the code around the pc and the thread's
/// registers (wire/fault.rs).
pub const RUN_SCHEMA: u32 = 2;

/// A function instance as a code record names it: the bytes of blaze-code's `record::Symbol`
/// (the item's stable id and its type arguments by content). This crate depends on no other,
/// so the wire carries the 32 bytes under its own name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Symbol(pub [u8; 32]);

/// What a need asks for: a slot the program numbered from the parent's [`Down::Declare`], or a
/// symbol the program has no slot for yet (a row of the run record, an offer's target).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Needed {
    Slot(Slot),
    Symbol(Symbol),
}

/// One pair the parent tells the program: `symbol`'s function runs through `slot`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Declared {
    pub symbol: Symbol,
    pub slot: Slot,
}

/// Why a need has no code: blaze-run's `Why`, plus the symbol the parent cannot resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure<'a> {
    /// The symbol names no function of the program's manifest.
    Unresolved,
    /// The type checker or codegen refused the body: its findings are this record.
    Refused { findings: [u8; 32] },
    /// The compile panicked (rule 20: this function alone fails).
    Panicked,
    /// A question the new layer cannot answer yet.
    NotBuilt { what: &'a str },
    /// The compile ran past its bound, in nanoseconds of the parent's clock.
    Overran { bound: u64 },
}

/// What the program says to its parent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Up {
    /// A stub fired: `thread` called what `need` names, which has no code here, and waits.
    /// Answered by a [`Down::Publish`] of the slot (after the [`Down::Declare`] that numbers it,
    /// for a symbol) or a [`Down::Failed`] naming the same need.
    Need { need: Needed, thread: Thread },
    /// The program dies on a fault or a trap, struck where `fault` says (wire/fault.rs): sent
    /// by the crash handler with id 0, once, just before the process ends by the signal. Nothing
    /// answers it; the parent keeps it to name the place by path and the trap by its reason.
    Fault { fault: super::fault::Fault },
}

/// What the parent says to the program.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Down<'a> {
    /// These symbols run through these slots: each slot's words are made to name its stub
    /// before any record naming the symbol is placed. Sent before the record that names them.
    Declare { pairs: List<'a, Declared> },
    /// Place the code record `record` (the store's, read in place) as `slot`'s body at `tier`
    /// (`Zero` is the fast tier), then wake the threads waiting on it.
    Publish { slot: Slot, tier: Tier, record: RecordId },
    /// `need` has no code, for `why`: each thread waiting on it fails its call (rule 20).
    Failed { need: Needed, why: Failure<'a> },
    /// `slot` is the program's `#[lang = "eh_personality"]` fn. Sent once, after the `Declare`
    /// naming the program's `eh_personality` fn and before any `Publish`: every placed record's
    /// unwind entry names that slot's entry thunk (wire/personality.rs).
    Personality { slot: Slot },
    /// The program's native library record `library` (NATIVE.md 3.1) and, in its word order, the
    /// Rust functions its C code calls: pair i's slot's entry thunk fills the library's word i.
    /// Sent once, first, before any `Declare` of the entry; each pair declares its slot as a
    /// `Declare` pair does (wire/native.rs).
    Native { library: RecordId, words: List<'a, Declared> },
    /// These statics, each its symbol and datum record, placed together before anything names
    /// them: sent before the `Declare` and `Publish` of the record that names them (run.md,
    /// "Statics in the run"; wire/data.rs).
    Data { data: List<'a, super::data::Datum> },
    /// The run's thread-local descriptor is `descriptor` (the `Space::Static` target of every
    /// record touching a thread-local) and its pthread key's destructor is the fn `ender`. Sent
    /// once, before the first `Publish` of a record naming the descriptor: the child makes the
    /// descriptor and the key (wire/thread_locals.rs).
    ThreadLocals { descriptor: Symbol, ender: Symbol },
}

impl Listed<'_> for Declared {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        // The keys in the order of their encodings: by length, then by bytes.
        to.map(2);
        to.text("slot");
        to.uint(u64::from(self.slot.0));
        to.text("symbol");
        to.bytes(&self.symbol.0);
    }

    fn read(from: &mut Reader<'_>) -> Result<Declared, WireError> {
        from.map(2, "a declared pair, a map of 2")?;
        from.key("slot")?;
        let slot = Slot(from.uint32("a slot past the 32 bits a table has")?);
        from.key("symbol")?;
        let symbol = Symbol(id(from, "a symbol is 32 bytes")?);
        Ok(Declared { symbol, slot })
    }
}

impl Needed {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.map(1);
        match *self {
            Needed::Slot(slot) => {
                to.text("Slot");
                to.uint(u64::from(slot.0));
            }
            Needed::Symbol(symbol) => {
                to.text("Symbol");
                to.bytes(&symbol.0);
            }
        }
    }

    fn read(from: &mut Reader<'_>) -> Result<Needed, WireError> {
        from.map(1, "a need, a map of one variant")?;
        let at = from.at();
        match from.text()? {
            "Slot" => Ok(Needed::Slot(Slot(from.uint32("a slot past the 32 bits a table has")?))),
            "Symbol" => Ok(Needed::Symbol(Symbol(id(from, "a symbol is 32 bytes")?))),
            _ => Err(WireError::Variant { at }),
        }
    }
}

impl<'a> Failure<'a> {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        match *self {
            Failure::Unresolved => variant(to, "Unresolved", 0),
            Failure::Refused { findings } => {
                variant(to, "Refused", 1);
                to.text("findings");
                to.bytes(&findings);
            }
            Failure::Panicked => variant(to, "Panicked", 0),
            Failure::NotBuilt { what } => {
                variant(to, "NotBuilt", 1);
                to.text("what");
                to.text(what);
            }
            Failure::Overran { bound } => {
                variant(to, "Overran", 1);
                to.text("bound");
                to.uint(bound);
            }
        }
    }

    fn read(from: &mut Reader<'a>) -> Result<Failure<'a>, WireError> {
        from.map(1, "a failure, a map of one variant")?;
        let at = from.at();
        let failure = match from.text()? {
            "Unresolved" => {
                from.map(0, "Unresolved, a map of 0")?;
                Failure::Unresolved
            }
            "Refused" => {
                from.map(1, "Refused, a map of 1")?;
                from.key("findings")?;
                Failure::Refused { findings: id(from, "a findings record's key is 32 bytes")? }
            }
            "Panicked" => {
                from.map(0, "Panicked, a map of 0")?;
                Failure::Panicked
            }
            "NotBuilt" => {
                from.map(1, "NotBuilt, a map of 1")?;
                from.key("what")?;
                Failure::NotBuilt { what: from.text()? }
            }
            "Overran" => {
                from.map(1, "Overran, a map of 1")?;
                from.key("bound")?;
                Failure::Overran { bound: from.uint()? }
            }
            _ => return Err(WireError::Variant { at }),
        };
        Ok(failure)
    }
}

impl Up {
    /// Writes the message as one frame onto the end of `out`. A message too long for a frame
    /// leaves `out` as it was.
    pub fn encode<A: Allocator>(&self, id: u64, out: &mut Vec<u8, A>) -> Result<(), WireError> {
        run_frame(id, out, |to| match *self {
            Up::Need { need, thread } => {
                variant(to, "Need", 2);
                to.text("need");
                need.write(to);
                to.text("thread");
                to.uint(thread.0);
            }
            Up::Fault { fault } => super::fault::write(to, &fault),
        })
    }

    /// Reads one frame's item (from [`super::split`]): its id and the message.
    pub fn decode(item: &[u8]) -> Result<(u64, Up), WireError> {
        let (id, mut from) = run_envelope(item)?;
        from.map(1, "a body, a map of one variant")?;
        let named = from.at();
        let up = match from.text()? {
            "Need" => {
                from.map(2, "Need, a map of 2")?;
                from.key("need")?;
                let need = Needed::read(&mut from)?;
                from.key("thread")?;
                let thread = Thread(from.uint()?);
                Up::Need { need, thread }
            }
            super::fault::NAME => Up::Fault { fault: super::fault::read(&mut from)? },
            _ => return Err(WireError::Variant { at: named }),
        };
        from.end()?;
        Ok((id, up))
    }
}

impl<'a> Down<'a> {
    /// Writes the message as one frame onto the end of `out`. A message too long for a frame
    /// leaves `out` as it was.
    pub fn encode<A: Allocator>(&self, id: u64, out: &mut Vec<u8, A>) -> Result<(), WireError> {
        run_frame(id, out, |to| match *self {
            Down::Declare { pairs } => {
                variant(to, "Declare", 1);
                to.text("pairs");
                pairs.write(to);
            }
            Down::Publish { slot, tier, record } => {
                variant(to, "Publish", 3);
                to.text("slot");
                to.uint(u64::from(slot.0));
                to.text("tier");
                to.text(tier_name(tier));
                to.text("record");
                to.bytes(&record.0);
            }
            Down::Personality { slot } => personality::write(to, slot),
            Down::Native { library, words } => native::write(to, library, words),
            Down::Data { data } => data::write(to, data),
            Down::ThreadLocals { descriptor, ender } => thread_locals::write(to, descriptor, ender),
            Down::Failed { need, why } => {
                variant(to, "Failed", 2);
                to.text("why");
                why.write(to);
                to.text("need");
                need.write(to);
            }
        })
    }

    /// Reads one frame's item (from [`super::split`]): its id and the message, borrowing from
    /// `item`.
    pub fn decode(item: &'a [u8]) -> Result<(u64, Down<'a>), WireError> {
        // Not `id`: the Publish arm calls the function of that name.
        let (message, mut from) = run_envelope(item)?;
        from.map(1, "a body, a map of one variant")?;
        let named = from.at();
        let down = match from.text()? {
            "Declare" => {
                from.map(1, "Declare, a map of 1")?;
                from.key("pairs")?;
                Down::Declare { pairs: List::read(&mut from)? }
            }
            "Publish" => {
                from.map(3, "Publish, a map of 3")?;
                from.key("slot")?;
                let slot = Slot(from.uint32("a slot past the 32 bits a table has")?);
                from.key("tier")?;
                let tier = tier(&mut from)?;
                from.key("record")?;
                let record = RecordId(id(&mut from, "a record id is 32 bytes")?);
                Down::Publish { slot, tier, record }
            }
            personality::NAME => Down::Personality { slot: personality::read(&mut from)? },
            native::NAME => {
                let (library, words) = native::read(&mut from)?;
                Down::Native { library, words }
            }
            data::NAME => Down::Data { data: data::read(&mut from)? },
            thread_locals::NAME => {
                let (descriptor, ender) = thread_locals::read(&mut from)?;
                Down::ThreadLocals { descriptor, ender }
            }
            "Failed" => {
                from.map(2, "Failed, a map of 2")?;
                from.key("why")?;
                let why = Failure::read(&mut from)?;
                from.key("need")?;
                let need = Needed::read(&mut from)?;
                Down::Failed { need, why }
            }
            _ => return Err(WireError::Variant { at: named }),
        };
        from.end()?;
        Ok((message, down))
    }
}

/// One frame onto the end of `out`: the length, then `{id, run, body}` with `body` writing the
/// body (the keys in the order of their encodings).
fn run_frame<A: Allocator>(
    id: u64,
    out: &mut Vec<u8, A>,
    body: impl FnOnce(&mut Writer<'_, A>),
) -> Result<(), WireError> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    let mut to = Writer::new(out);
    to.map(3);
    to.text("id");
    to.uint(id);
    to.text("run");
    to.uint(u64::from(RUN_SCHEMA));
    to.text("body");
    body(&mut to);
    let len = (out.len() - start - 4) as u64;
    if len > u64::from(LONGEST_FRAME) {
        out.truncate(start);
        return Err(WireError::Long { len });
    }
    out[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(())
}

/// Reads a run message's envelope up to its body: the id, and the reader at the body. The
/// schema comes before the body, so a message of another schema is refused as that, not as a
/// malformed body; the caller reads the body and then [`Reader::end`].
fn run_envelope(item: &[u8]) -> Result<(u64, Reader<'_>), WireError> {
    let mut from = Reader::new(item);
    from.map(3, "a run message, a map of 3")?;
    from.key("id")?;
    let id = from.uint()?;
    from.key("run")?;
    let said = from.uint()?;
    if said != u64::from(RUN_SCHEMA) {
        return Err(WireError::RunSchema { said });
    }
    from.key("body")?;
    Ok((id, from))
}
