//! The run mode's statics in simulation (run.md, "Statics in the run", steps 1 and 2): a fake
//! parent sends `Data` before the record that names a static, and a memory keeps what each
//! publication of a static wrote.
//!
//! The rule, the Rust Reference, "Static items" [items.static.intro]: "A static item is similar
//! to a constant, except that it represents an allocation in the program that is initialized
//! with the initializer expression. All references and raw pointers to the static refer to the
//! same allocation." So a static is placed once, before any code loads its address, and every
//! record naming it reads the one word that holds that address; a static whose value holds
//! another static's address holds the address that static was placed at.

use blaze_code::a64::adrp_ldr_target;
use blaze_code::datum::{Datum, OWN_DATUM, Place};
use blaze_code::record::{CodeRecord, Site, SiteKind, Space, Symbol, Target, Word};
use blaze_launch::wire::data::Datum as Told;
use blaze_launch::wire::run::{Declared, Down, Symbol as WireSymbol};
use blaze_launch::wire::{List, RecordId};
use blaze_launch::{Slot, Thread, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::{Nanos, SlotId};

use super::{Act, Event, Memory, Outside, Placing, Plant, RecordSource, RunMode, RunModeError};
use crate::code_unwind::Step;

const ADRP_X0: u32 = 0x9000_0000;
const LDR_X0: u32 = 0xF940_0000;
const RET: u32 = 0xD65F_03C0;

/// The simulated reservation, as run_mode/tests.rs lays it out; the statics' words below the
/// slot words.
const WORDS: u64 = 0x1_0000_0000;
const ADDRESS_WORDS: u64 = WORDS + 0x80_0000;
const THUNKS: u64 = WORDS + 0x100_0000;
const AREA: u64 = WORDS + 0x4000_0000;

/// The entry, and two statics: `S`, plain bytes, and `T`, whose value holds `S`'s address, its
/// own byte 8's, and the entry fn's.
const F: Symbol = Symbol([1; 32]);
const S: Symbol = Symbol([5; 32]);
const T: Symbol = Symbol([4; 32]);
const F_WIRE: Slot = Slot(7);
const HELLO: &[u8; 16] = b"Hello, world!\n\0\0";

/// `f`: `adrp x0, S@PAGE; ldr x0, [x0, S@PAGEOFF]; ret`: `S`'s address in `x0`.
fn code(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X0, LDR_X0, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let s = r.target(Target { space: Space::Static, symbol: S });
    r.sites.push(Site { at: 0, kind: SiteKind::Load, target: s, addend: 0 });
    let sealed = r.seal().expect("the record seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// A datum: `place`, align 8, `bytes`, and `words` as (offset, target or own, addend).
fn datum(place: Place, bytes: &[u8], words: &[(u32, Option<Target>, i64)], alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut d = Datum::new_in(place, 3, alloc);
    d.bytes.extend_from_slice(bytes);
    for &(at, target, addend) in words {
        let target = match target {
            Some(target) => d.target(target),
            None => OWN_DATUM,
        };
        d.words.push(Word { at, target, addend });
    }
    let sealed = d.seal().expect("the datum seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// The store: records by id.
struct Store {
    records: AVec<'static, (RecordId, AVec<'static, u8>)>,
}

impl Store {
    fn put(&mut self, bytes: AVec<'static, u8>) -> RecordId {
        let id = RecordId(blaze_blake3::hash(&bytes));
        self.records.push((id, bytes));
        id
    }
}

impl RecordSource for Store {
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
        match self.records.iter().find(|(it, _)| *it == id) {
            Some((_, bytes)) => Ok(bytes),
            None => Err(RunModeError::Missing { record: id, errno: 2 }),
        }
    }
}

/// The memory: each static published (its word's number, address, bytes), the code each Copy
/// wrote, and the order of publications and stores.
struct Sim {
    next: u64,
    published: AVec<'static, (u32, u64, AVec<'static, u8>)>,
    copied: AVec<'static, (u64, AVec<'static, u8>)>,
    /// "static" or "store", in order: no record's word is stored before the statics it names.
    order: AVec<'static, &'static str>,
    alloc: Alloc<'static>,
}

impl Memory for Sim {
    fn stub(&mut self, _slot: SlotId) -> Result<(), RunModeError> {
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

    fn outside(&mut self, symbol: Symbol, _name: &str) -> Result<Outside, RunModeError> {
        Err(RunModeError::Outside { symbol, why: "the simulation binds no C names" })
    }

    fn reserve(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.next.next_multiple_of(align as u64);
        self.next = at + len as u64;
        Ok(at)
    }

    fn reserve_hot(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        self.reserve(len, align)
    }

    fn carry(&mut self, step: Step, placing: &Placing<'_>) -> Result<(), RunModeError> {
        match step {
            Step::Copy => {
                let mut kept = Vec::new_in(self.alloc);
                kept.extend_from_slice(placing.code);
                self.copied.push((placing.code_at, kept));
            }
            Step::Store => self.order.push("store"),
            Step::Register | Step::Flush => {}
        }
        Ok(())
    }

    fn static_word(&self, index: u32) -> u64 {
        WORDS - 8 * (u64::from(index) + 1)
    }

    fn publish_static(&mut self, index: u32, at: u64, bytes: &[u8]) -> Result<(), RunModeError> {
        let mut kept = Vec::new_in(self.alloc);
        kept.extend_from_slice(bytes);
        self.published.push((index, at, kept));
        self.order.push("static");
        Ok(())
    }
}

/// What one run showed: the placement's result (or the frame's refusal), and the memory.
struct Seen {
    result: Result<(), RunModeError>,
    memory: Sim,
}

/// The entry's stub fired; then the parent's `Data` (when `told` holds any), `Declare` and
/// `Publish` of `f`'s record, the acts carried as the native driver does, to the first error.
fn run(statics: &[(Symbol, AVec<'static, u8>)], twice: bool) -> Seen {
    let alloc = Alloc::global();
    let mut store = Store { records: Vec::new_in(alloc) };
    let f = store.put(code(alloc));
    let mut told: AVec<'static, Told> = Vec::new_in(alloc);
    for (symbol, bytes) in statics {
        let mut copy = Vec::new_in(alloc);
        copy.extend_from_slice(bytes);
        told.push(Told { symbol: WireSymbol(symbol.0), record: store.put(copy) });
    }
    let mut memory = Sim { next: AREA, published: Vec::new_in(alloc), copied: Vec::new_in(alloc), order: Vec::new_in(alloc), alloc };
    let mut mode = RunMode::new(alloc);
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc);
    let entry = mode.start(F, &mut acts).expect("the entry is numbered");
    mode.on(Event::Tick(Nanos(1)), &mut acts).expect("a tick");
    mode.on(Event::Fired { thread: Thread(1), slot: entry }, &mut acts).expect("the entry's stub fires");
    mode.sent();
    acts.clear();

    let mut down = std::vec::Vec::new();
    if !told.is_empty() {
        Down::Data { data: List::of(&told) }.encode(1, &mut down).expect("Data encodes");
        if twice {
            Down::Data { data: List::of(&told[..1]) }.encode(2, &mut down).expect("Data encodes");
        }
    }
    let pairs = [Declared { symbol: WireSymbol(F.0), slot: F_WIRE }];
    Down::Declare { pairs: List::of(&pairs) }.encode(3, &mut down).expect("Declare encodes");
    Down::Publish { slot: F_WIRE, tier: Tier::Zero, record: f }.encode(4, &mut down).expect("Publish encodes");
    if let Err(why) = mode.on(Event::Frame(&down), &mut acts) {
        return Seen { result: Err(why), memory };
    }
    let mut result = Ok(());
    let mut next = 0;
    while next < acts.len() && result.is_ok() {
        let act = acts[next];
        next += 1;
        result = match act {
            Act::Stub { slot } => memory.stub(slot),
            Act::Statics => mode.place_statics(&store, &mut memory),
            Act::Place { slot, record } => mode.place(slot, record, &store, &mut memory, Plant::None, &mut acts),
            Act::Wake { .. } => Ok(()),
            Act::Promote { .. } => panic!("this simulation publishes no optimized record"),
            Act::Stop(stop) => panic!("the run stopped: {stop:?}"),
            Act::Native { record } => panic!("this simulation sends no native library: {record:?}"),
        };
    }
    Seen { result, memory }
}

/// Steps 1 and 2: `S` (plain bytes) and `T` (its value `S`'s address, its own byte 8's, and
/// `f`'s thunk) are placed before `f`, together; `f`'s load reads `S`'s word; each word of `T`
/// holds what it names. Controls: `f` published with no `Data` is refused naming `S`; the same
/// static sent twice is refused; a writable static is refused by name (step 3 to come).
/// Prints "run mode: 2 statics placed before the record naming them; its load reads S's word;
/// T holds S, itself and f; 3 controls refused".
#[test]
fn a_static_is_placed_before_the_record_that_names_it() {
    let alloc = Alloc::global();
    let s = datum(Place::ReadOnly, HELLO, &[], alloc);
    let t = datum(
        Place::ReadOnly,
        &[0; 24],
        &[(0, Some(Target { space: Space::Static, symbol: S }), 0), (8, None, 8), (16, Some(Target { space: Space::Func, symbol: F }), 0)],
        alloc,
    );
    // `T` first: a word naming a static later in the same batch resolves.
    let seen = run(&[(T, t), (S, s)], false);
    assert_eq!(seen.result, Ok(()), "the run placed its statics and f");
    let m = &seen.memory;
    assert_eq!(m.published.len(), 2, "both statics published");
    assert_eq!(m.order.first().copied(), Some("static"), "a static before any record's word: {:?}", m.order);
    assert_eq!(m.order.iter().filter(|it| **it == "static").count(), 2);
    assert_eq!(m.order.last().copied(), Some("store"), "f's word last: {:?}", m.order);
    let (t_index, t_at, t_bytes) = &m.published[0];
    let (s_index, s_at, s_bytes) = &m.published[1];
    assert_eq!(&s_bytes[..], &HELLO[..], "S's bytes are its initializer's");
    assert_eq!(*s_at % 8, 0, "S is aligned as its datum says");
    let word = |at: usize| u64::from_le_bytes(t_bytes[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!(word(0), *s_at, "T's first word holds S's address");
    assert_eq!(word(8), t_at + 8, "T's second word holds its own byte 8's address");
    assert_eq!(word(16), m.thunk(SlotId(0)), "T's third word holds f's thunk (f is slot 0)");
    assert_ne!(t_index, s_index);

    let (code_at, code) = &m.copied[0];
    let first = u32::from_le_bytes(code[0..4].try_into().expect("4 bytes"));
    let second = u32::from_le_bytes(code[4..8].try_into().expect("4 bytes"));
    assert_eq!(adrp_ldr_target(first, second, *code_at), Some(m.static_word(*s_index)), "f's load reads S's word");

    // Control: the record without the `Data` before it.
    let bare = run(&[], false);
    assert!(
        matches!(bare.result, Err(RunModeError::StaticNotTold { symbol, .. }) if symbol == S),
        "a record naming an untold static: {:?}",
        bare.result
    );
    assert!(bare.memory.copied.is_empty() && bare.memory.order.is_empty(), "nothing of f was written");
    // Control: a static told twice.
    let again = run(&[(S, datum(Place::ReadOnly, HELLO, &[], alloc))], true);
    assert!(matches!(again.result, Err(RunModeError::StaticTwice { .. })), "a static sent twice: {:?}", again.result);
    // Control: a writable static.
    let writable = run(&[(S, datum(Place::Writable, HELLO, &[], alloc))], false);
    assert!(matches!(writable.result, Err(RunModeError::NotBuilt { .. })), "a writable static: {:?}", writable.result);
    println!("run mode: 2 statics placed before the record naming them; its load reads S's word; T holds S, itself and f; 3 controls refused");
}
