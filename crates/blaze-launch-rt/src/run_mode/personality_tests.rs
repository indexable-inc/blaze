//! The run mode's personality binding in simulation (piece P3 of port27/tempo-run/UNWIND.md): a
//! fake parent, an in-memory record with a cleanup row, and a memory that keeps each placed
//! entry's bytes as the Copy step wrote them.
//!
//! The rule, the Rust Reference, "Panic", [panic.unwind.destruction]: "When a panic occurs, the
//! unwind handler “unwinds” Rust frames, just as C++’s throw unwinds C++ frames, until the panic
//! reaches the point of recovery ... as the panic traverses Rust frames, live objects in those
//! frames that implement Drop will have their drop methods called." On this target the frames'
//! cleanups are found by the system's unwinder through each frame's CIE, whose `P` augmentation
//! names the personality routine (the pinned library's std/src/sys/personality/gcc.rs:
//! `#[lang = "eh_personality"] unsafe extern "C" fn rust_eh_personality(..)`), which reads the
//! LSDA's call sites and lands in the cleanup. A frame whose entry named no routine, or the
//! wrong one, would skip its drops (a leak where the Reference says the drop runs): so an entry
//! with a landing row is never made without the program's routine, and the routine it names is
//! that fn's entry thunk.

use blaze_code::eh::EhError;
use blaze_code::record::{CodeRecord, Pad, Site, SiteKind, Space, Symbol, Target};
use blaze_code::unwind::{Action, Frame};
use blaze_launch::wire::run::{Declared, Down, Symbol as WireSymbol};
use blaze_launch::wire::{List, RecordId};
use blaze_launch::{Slot, Thread, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::{Nanos, SlotId};

use super::{Act, Event, Memory, Outside, Placing, Plant, RecordSource, RunMode, RunModeError, ThreadKey};
use crate::code_unwind::Step;

const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const RET: u32 = 0xD65F_03C0;

/// The simulated reservation, as run_mode/tests.rs lays it out.
const WORDS: u64 = 0x1_0000_0000;
const ADDRESS_WORDS: u64 = WORDS + 0x80_0000;
const THUNKS: u64 = WORDS + 0x100_0000;
const AREA: u64 = WORDS + 0x4000_0000;

/// The entry, the callee, and the program's `eh_personality` fn.
const F: Symbol = Symbol([1; 32]);
const G: Symbol = Symbol([2; 32]);
const P: Symbol = Symbol([9; 32]);

/// The parent's slots for `f` and the personality.
const F_WIRE: Slot = Slot(7);
const P_WIRE: Slot = Slot(3);

/// DWARF's `DW_EH_PE_absptr`, the encoding eh.rs writes every address in.
const ABSPTR: u8 = 0x00;

/// `f`: `call g; ret` with the plain frame (prologue 8 bytes), and ONE row: the call at 8 lands
/// at the `ret` at 12 as a cleanup (what blaze-build's `call_unwinding` makes for a call with a
/// value live across it).
fn record(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X16, LDR_X16, BLR_X16, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let g = r.target(Target { space: Space::Func, symbol: G });
    r.sites.push(Site { at: 0, kind: SiteKind::Call, target: g, addend: 0 });
    r.frame = Frame::PLAIN.word().expect("the plain frame has a word");
    r.pads.push(Pad { start: 8, len: 4, landing: 12, action: Action::Cleanup.code() });
    let sealed = r.seal().expect("the record seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// The store: one record.
struct One {
    id: RecordId,
    bytes: AVec<'static, u8>,
}

impl RecordSource for One {
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
        match id == self.id {
            true => Ok(&self.bytes),
            false => Err(RunModeError::Missing { record: id, errno: 2 }),
        }
    }
}

/// The memory: the stubs written, and the entry bytes each Copy step wrote, by slot.
struct Sim {
    next: u64,
    stubbed: AVec<'static, SlotId>,
    entries: AVec<'static, (SlotId, AVec<'static, u8>)>,
    registered: AVec<'static, SlotId>,
    alloc: Alloc<'static>,
}

impl Memory for Sim {
    fn stub(&mut self, slot: SlotId) -> Result<(), RunModeError> {
        self.stubbed.push(slot);
        Ok(())
    }

    fn word(&self, slot: SlotId) -> u64 {
        WORDS + 8 * u64::from(slot.0)
    }

    fn address_word(&self, slot: SlotId) -> u64 {
        ADDRESS_WORDS + 8 * u64::from(slot.0)
    }

    fn thunk(&self, slot: SlotId) -> u64 {
        THUNKS + 16 * u64::from(slot.0)
    }

    fn outside(&mut self, record: RecordId, symbol: Symbol, name: &str) -> Result<Outside, RunModeError> {
        Err(RunModeError::Outside { record, symbol, name: super::CName::new(name), why: "the simulation binds no C names" })
    }

    fn reserve(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.next.next_multiple_of(align as u64);
        self.next = at + len as u64;
        Ok(at)
    }

    /// The model has one range: hot code is laid out after the rest, as it comes.
    fn reserve_hot(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        self.reserve(len, align)
    }

    fn carry(&mut self, step: Step, placing: &Placing<'_>) -> Result<(), RunModeError> {
        match step {
            Step::Copy => {
                if let Some((_, bytes, _)) = placing.entry {
                    let mut kept = Vec::new_in(self.alloc);
                    kept.extend_from_slice(bytes);
                    self.entries.push((placing.slot, kept));
                }
            }
            Step::Register => self.registered.push(placing.slot),
            Step::Flush | Step::Store => {}
        }
        Ok(())
    }

    fn static_word(&self, index: u32) -> u64 {
        WORDS - 8 * (u64::from(index) + 1)
    }

    fn reserve_data(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "the personality tests place no static" })
    }

    fn publish_static(&mut self, _index: u32, _at: u64, _bytes: &[u8], _room: super::Room) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "the personality tests place no static" })
    }

    fn thread_key(&mut self, _ender: u64) -> Result<ThreadKey, RunModeError> {
        Err(RunModeError::NotBuilt { what: "the personality tests make no thread-local key" })
    }

    fn write(&mut self, _at: u64, _bytes: &[u8]) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "the personality tests write no thread-local row" })
    }
}

/// What one run showed: the first placement's result, and the memory.
struct Seen {
    placed: Result<(), RunModeError>,
    memory: Sim,
    /// The local slot the parent's `P_WIRE` was declared as.
    p_local: SlotId,
}

/// One stub of `f` fired, then the parent's `Declare` (f as 7, the personality as 3), the
/// `Personality` message when `bind` says, and the `Publish` of `f`'s record; the acts carried
/// as the native driver does, until the placement's result.
fn run(bind: bool) -> Seen {
    let alloc = Alloc::global();
    let bytes = record(alloc);
    let source = One { id: RecordId(blaze_blake3::hash(&bytes)), bytes };
    let mut memory = Sim {
        next: AREA,
        stubbed: Vec::new_in(alloc),
        entries: Vec::new_in(alloc),
        registered: Vec::new_in(alloc),
        alloc,
    };
    let mut mode = RunMode::new(alloc);
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc);

    let entry = mode.start(F, &mut acts).expect("the entry is numbered");
    assert_eq!(entry, SlotId(0), "the entry is slot 0");
    mode.on(Event::Tick(Nanos(1)), &mut acts).expect("a tick");
    mode.on(Event::Fired { thread: Thread(1), slot: entry }, &mut acts).expect("the entry's stub fires");
    mode.sent();

    let mut down = std::vec::Vec::new();
    let pairs = [Declared { symbol: WireSymbol(F.0), slot: F_WIRE }, Declared { symbol: WireSymbol(P.0), slot: P_WIRE }];
    Down::Declare { pairs: List::of(&pairs) }.encode(1, &mut down).expect("Declare encodes");
    if bind {
        Down::Personality { slot: P_WIRE }.encode(2, &mut down).expect("Personality encodes");
    }
    Down::Publish { slot: F_WIRE, tier: Tier::Zero, record: source.id }.encode(3, &mut down).expect("Publish encodes");
    mode.on(Event::Frame(&down), &mut acts).expect("the parent's frames read");
    let p_local = SlotId(1);

    let mut placed = Err(RunModeError::NotBuilt { what: "the test saw no placement" });
    let mut next = 0;
    while next < acts.len() {
        let act = acts[next];
        next += 1;
        match act {
            Act::Stub { slot } => memory.stub(slot).expect("a stub"),
            Act::Place { slot, record } => {
                placed = mode.place(slot, record, &source, &mut memory, Plant::None, &mut acts);
                if placed.is_err() {
                    break;
                }
            }
            Act::Wake { .. } => {}
            Act::Promote { .. } => panic!("the personality tests publish no optimized record"),
            Act::Stop(stop) => panic!("the run stopped: {stop:?}"),
            Act::Native { record } => panic!("the personality tests send no native library: {record:?}"),
            Act::Statics => panic!("the personality tests send no static"),
            Act::ThreadLocals => panic!("the personality tests send no thread-local"),
            Act::Hook { slot, kind } => mode.hook(slot, kind, &mut memory).expect("hook"),
        }
    }
    Seen { placed, memory, p_local }
}

/// One unsigned LEB128 number at `bytes[*at..]`, `*at` moved past it.
fn uleb(bytes: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0u32);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// The personality routine's address a CIE names, read the way an unwinder reads it: the
/// length and the CIE id, the version, the augmentation string (which must be `zPLR`: a
/// personality, then an LSDA and an FDE encoding), the code and data alignment factors and the
/// return-address register, the augmentation data's length, then the `P` part: its encoding
/// (`absptr`) and the eight bytes of the address.
fn cie_personality(entry: &[u8]) -> u64 {
    assert_eq!(&entry[4..8], &[0; 4], "the entry starts with a CIE (id 0)");
    assert_eq!(entry[8], 1, "CIE version 1");
    let end = 9 + entry[9..].iter().position(|&b| b == 0).expect("the augmentation string ends");
    assert_eq!(&entry[9..end], b"zPLR", "a CIE of an entry with a landing row names a personality");
    let mut at = end + 1;
    uleb(entry, &mut at); // code alignment factor
    uleb(entry, &mut at); // data alignment factor (an SLEB: one byte either way, skipped alike)
    uleb(entry, &mut at); // return address register
    let data = uleb(entry, &mut at);
    assert_eq!(data, 1 + 8 + 1 + 1, "the augmentation data: P (encoding and address), L, R");
    assert_eq!(entry[at], ABSPTR, "the personality's address is an absptr");
    let word: [u8; 8] = entry[at + 1..at + 9].try_into().expect("eight bytes");
    u64::from_le_bytes(word)
}

/// A record whose row lands (a cleanup) is refused by name while the parent has named no
/// personality (`EhError::NoPersonality`: no entry is made, nothing is copied or registered,
/// so no frame the unwinder cannot run the cleanup of is ever reachable); after the parent's
/// `Personality` the same record is placed, and its entry's CIE names the personality slot's
/// ENTRY THUNK (never a body, never 0: the routine's body is compiled on the first throw). A
/// second `Personality` is refused (`PersonalityTwice`), and one naming an undeclared slot is
/// `Undeclared`. Prints "run_mode: personality bound once; refused before, placed after".
#[test]
fn a_landing_record_is_refused_before_the_personality_and_names_its_thunk_after() {
    let before = run(false);
    let record = match before.placed {
        Err(RunModeError::Eh { record, why: EhError::NoPersonality }) => record,
        other => panic!("before `Personality` a landing record must be refused as NoPersonality, got {other:?}"),
    };
    assert!(before.memory.entries.is_empty(), "nothing copied for the refused record");
    assert!(before.memory.registered.is_empty(), "nothing registered for the refused record");

    let after = run(true);
    assert_eq!(after.placed, Ok(()), "after `Personality` record {record:?} places");
    assert_eq!(after.memory.registered.as_slice(), &[SlotId(0)], "f's entry registered once");
    assert_eq!(after.memory.entries.len(), 1, "one entry copied");
    let (slot, entry) = &after.memory.entries[0];
    assert_eq!(*slot, SlotId(0), "the entry is f's");
    assert!(after.memory.stubbed.contains(&after.p_local), "the personality's slot has its stub and thunk");
    let named = cie_personality(entry);
    assert_eq!(named, after.memory.thunk(after.p_local), "the CIE names the personality slot's entry thunk");
    assert_ne!(named, after.memory.thunk(SlotId(0)), "not f's own thunk");

    // A second `Personality`, and one for a slot never declared, each in a fresh run.
    let alloc = Alloc::global();
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc);
    let mut mode = RunMode::new(alloc);
    mode.start(F, &mut acts).expect("the entry is numbered");
    let mut down = std::vec::Vec::new();
    let pairs = [Declared { symbol: WireSymbol(P.0), slot: P_WIRE }];
    Down::Declare { pairs: List::of(&pairs) }.encode(1, &mut down).expect("Declare encodes");
    Down::Personality { slot: P_WIRE }.encode(2, &mut down).expect("Personality encodes");
    mode.on(Event::Frame(&down), &mut acts).expect("Declare and one Personality read");
    down.clear();
    Down::Personality { slot: P_WIRE }.encode(3, &mut down).expect("Personality encodes");
    assert_eq!(mode.on(Event::Frame(&down), &mut acts), Err(RunModeError::PersonalityTwice { slot: P_WIRE }));

    let mut fresh = RunMode::new(alloc);
    fresh.start(F, &mut acts).expect("the entry is numbered");
    down.clear();
    Down::Personality { slot: Slot(40) }.encode(1, &mut down).expect("Personality encodes");
    assert_eq!(fresh.on(Event::Frame(&down), &mut acts), Err(RunModeError::Undeclared { slot: Slot(40) }));
    println!("run_mode: personality bound once; refused before, placed after");
}
