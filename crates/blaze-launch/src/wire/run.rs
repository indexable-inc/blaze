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
use super::{LONGEST_FRAME, List, Listed, RecordId, WireError, bytes, data, id, native, personality, thread_locals, tier, tier_name, variant};
use crate::{Slot, Thread, Tier};

/// The version of the run wire's bodies. A reader of another refuses the message by name
/// ([`WireError::RunSchema`]). 2: `Up::Fault` carries the code around the pc and the thread's
/// registers (wire/fault.rs). 3: `Up::Fault` carries the faulting thread's callers, its
/// frame-pointer chain (wire/fault.rs `Callers`). 4: `Down::Positioned` publishes a code
/// record with this run's source-position values. 5: `Down::Hooks` toggles observe/span stubs
/// dynamically on a live run and `Down::Promote` carries an optimized record's inlined callee
/// read list for automatic hook-toggle deoptimization. 6: `Down::Bytes` carries a record's bytes
/// inline, in parts, before the message that names it (wire/bytes.rs).
pub const RUN_SCHEMA: u32 = 6;

pub use super::bytes::{BYTES_HELD_MOST, BYTES_PART_MOST, BYTES_RECORD_MOST};

/// What observe stub (or plain execution) [`Down::Hooks`] selects for a slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HookKind {
    /// Remove any observe stub and jump straight to the slot's effective body.
    Plain,
    /// Per-function call counter stub.
    Count,
    /// Per-thread entry-mark bitmap stub.
    Mark,
    /// Entry-only runtime hook stub.
    Hook,
    /// Entry+exit shadow-stack span hook stub (records duration and parent-child span tree).
    Span,
    /// Rust global-allocator slot hook (`__rust_alloc*` / `__rdl_*`).
    Alloc(crate::telemetry::AllocHookKind),
}

impl HookKind {
    fn name(self) -> &'static str {
        use crate::telemetry::AllocHookKind;
        match self {
            HookKind::Plain => "plain",
            HookKind::Count => "count",
            HookKind::Mark => "mark",
            HookKind::Hook => "hook",
            HookKind::Span => "span",
            HookKind::Alloc(AllocHookKind::Alloc) => "rust_alloc",
            HookKind::Alloc(AllocHookKind::Dealloc) => "rust_dealloc",
            HookKind::Alloc(AllocHookKind::Realloc) => "rust_realloc",
            HookKind::Alloc(AllocHookKind::AllocZeroed) => "rust_alloc_zeroed",
        }
    }

    fn read(from: &mut Reader<'_>) -> Result<HookKind, WireError> {
        use crate::telemetry::AllocHookKind;
        let at = from.at();
        match from.text()? {
            "plain" => Ok(HookKind::Plain),
            "count" => Ok(HookKind::Count),
            "mark" => Ok(HookKind::Mark),
            "hook" => Ok(HookKind::Hook),
            "span" => Ok(HookKind::Span),
            "rust_alloc" => Ok(HookKind::Alloc(AllocHookKind::Alloc)),
            "rust_dealloc" => Ok(HookKind::Alloc(AllocHookKind::Dealloc)),
            "rust_realloc" => Ok(HookKind::Alloc(AllocHookKind::Realloc)),
            "rust_alloc_zeroed" => Ok(HookKind::Alloc(AllocHookKind::AllocZeroed)),
            _ => Err(WireError::Variant { at }),
        }
    }
}

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
    /// Answered by a [`Down::Publish`] or [`Down::Positioned`] of the slot (after the
    /// [`Down::Declare`] that numbers it, for a symbol) or a [`Down::Failed`] naming the same need.
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
    /// Place the same record with one little-endian u32 per sealed source-position row.
    /// The values describe this checkout and travel with this publication only.
    Positioned { slot: Slot, tier: Tier, record: RecordId, values: &'a [u8] },
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
    ///
    /// THE ONE RULE of when it is sent: exactly when the program's image holds C, C++ or
    /// assembly of its crates (an object or archive member, from a build script's outputs or a
    /// `#[link(kind = "static")]`). Then the parent names the record in the child's start
    /// (`BLAZE_LAUNCH_NATIVE`, its 64 hex digits) and writes this as its first frame; the child
    /// hands the entry over only once it has loaded it (blaze-launch-rt run_mode.rs `run`). A
    /// program with no native input gets neither: its image (which still exists, linking the
    /// system library, as the record its start list is rooted beside) is not opened, so its
    /// start costs nothing new. A `Native` the start did not name is refused by name.
    Native { library: RecordId, words: List<'a, Declared> },
    /// Place these statics together, each by symbol and datum record. An on-demand answer sends
    /// its new function slots in `Declare`, then its statics in `Data`, then the code naming
    /// them in `Publish` or `Positioned` (wire/data.rs).
    Data { data: List<'a, super::data::Datum> },
    /// The run's thread-local descriptor is `descriptor` (the `Space::Static` target of every
    /// record touching a thread-local) and its pthread key's destructor is the fn `ender`. Sent
    /// once, before the first `Publish` of a record naming the descriptor: the child makes the
    /// descriptor and the key (wire/thread_locals.rs).
    ThreadLocals { descriptor: Symbol, ender: Symbol },
    /// Toggle a live observe stub on or off for `slot`. Putting a stub on a callee also
    /// retreats any optimized caller that inlined it back to its fast body until `Plain`.
    Hooks { slot: Slot, kind: HookKind },
    /// Place an optimized record for `slot` with its inlined callee read list (`inlined`,
    /// wire slot numbers) and optional source-position `values`. If any slot in `inlined` is
    /// currently observed, the optimized body is kept ready but `slot` stays on its fast body
    /// until every inlined callee returns to plain.
    Promote { slot: Slot, record: RecordId, inlined: List<'a, Slot>, values: &'a [u8] },
    /// Bytes `at..at + part.len()` of record `record`, `total` bytes long (wire/bytes.rs): a
    /// record's parts come consecutively, in order, before the first message naming the record
    /// (`Data`, `Publish`, `Positioned`, `Promote`), once a run. The program assembles them
    /// under a bound and checks the whole against `record` before it places a byte.
    Bytes { record: RecordId, at: u64, total: u64, part: &'a [u8] },
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
                publication_fields(to, slot, tier, record);
            }
            Down::Positioned { slot, tier, record, values } => {
                variant(to, "Positioned", 4);
                publication_fields(to, slot, tier, record);
                to.text("values");
                to.bytes(values);
            }
            Down::Personality { slot } => personality::write(to, slot),
            Down::Native { library, words } => native::write(to, library, words),
            Down::Data { data } => data::write(to, data),
            Down::ThreadLocals { descriptor, ender } => thread_locals::write(to, descriptor, ender),
            Down::Hooks { slot, kind } => {
                variant(to, "Hooks", 2);
                to.text("kind");
                to.text(kind.name());
                to.text("slot");
                to.uint(u64::from(slot.0));
            }
            Down::Promote { slot, record, inlined, values } => {
                variant(to, "Promote", 4);
                to.text("slot");
                to.uint(u64::from(slot.0));
                to.text("record");
                to.bytes(&record.0);
                to.text("values");
                to.bytes(values);
                to.text("inlined");
                inlined.write(to);
            }
            Down::Bytes { record, at, total, part } => bytes::write(to, record, at, total, part),
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
            name @ ("Publish" | "Positioned") => {
                if name == "Publish" {
                    from.map(3, "Publish, a map of 3")?;
                } else {
                    from.map(4, "Positioned, a map of 4")?;
                }
                from.key("slot")?;
                let slot = Slot(from.uint32("a slot past the 32 bits a table has")?);
                from.key("tier")?;
                let tier = tier(&mut from)?;
                from.key("record")?;
                let record = RecordId(id(&mut from, "a record id is 32 bytes")?);
                if name == "Publish" {
                    Down::Publish { slot, tier, record }
                } else {
                    from.key("values")?;
                    Down::Positioned { slot, tier, record, values: from.bytes()? }
                }
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
            "Hooks" => {
                from.map(2, "Hooks, a map of 2")?;
                from.key("kind")?;
                let kind = HookKind::read(&mut from)?;
                from.key("slot")?;
                let slot = Slot(from.uint32("a slot past the 32 bits a table has")?);
                Down::Hooks { slot, kind }
            }
            "Promote" => {
                from.map(4, "Promote, a map of 4")?;
                from.key("slot")?;
                let slot = Slot(from.uint32("a slot past the 32 bits a table has")?);
                from.key("record")?;
                let record = RecordId(id(&mut from, "a record id is 32 bytes")?);
                from.key("values")?;
                let values = from.bytes()?;
                from.key("inlined")?;
                let inlined = List::read(&mut from)?;
                Down::Promote { slot, record, inlined, values }
            }
            bytes::NAME => {
                let (record, at, total, part) = bytes::read(&mut from)?;
                Down::Bytes { record, at, total, part }
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

fn publication_fields<A: Allocator>(to: &mut Writer<'_, A>, slot: Slot, tier: Tier, record: RecordId) {
    to.text("slot");
    to.uint(u64::from(slot.0));
    to.text("tier");
    to.text(tier_name(tier));
    to.text("record");
    to.bytes(&record.0);
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
