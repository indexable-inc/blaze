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

use super::{Act, Event, Memory, Outside, Placing, Plant, RecordSource, Room, RunMode, RunModeError, ThreadKey};
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
    stubbed: AVec<'static, SlotId>,
    published: AVec<'static, (u32, u64, AVec<'static, u8>)>,
    copied: AVec<'static, (u64, AVec<'static, u8>)>,
    /// "static", "store", "key" or "write", in order: no record's word is stored before the
    /// statics and rows it names.
    order: AVec<'static, &'static str>,
    /// Each [`Memory::write`]: its address and bytes.
    written: AVec<'static, (u64, AVec<'static, u8>)>,
    /// The destructor the key was made with.
    ender: Option<u64>,
    /// The next free byte of the simulated data area ([`DATA`] up).
    data_next: u64,
    /// The room of each static published, in [`Sim::published`]'s order.
    rooms: AVec<'static, Room>,
    alloc: Alloc<'static>,
}

/// The simulated writable data area: apart from the reservation, as the native one is.
const DATA: u64 = 0x9_0000_0000;

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

    fn reserve_data(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.data_next.next_multiple_of(align as u64);
        self.data_next = at + len as u64;
        Ok(at)
    }

    fn publish_static(&mut self, index: u32, at: u64, bytes: &[u8], room: Room) -> Result<(), RunModeError> {
        let mut kept = Vec::new_in(self.alloc);
        kept.extend_from_slice(bytes);
        self.published.push((index, at, kept));
        self.rooms.push(room);
        self.order.push("static");
        Ok(())
    }

    fn thread_key(&mut self, ender: u64) -> Result<ThreadKey, RunModeError> {
        self.ender = Some(ender);
        self.order.push("key");
        Ok(ThreadKey { key: KEY, root: KEY * 8 })
    }

    fn write(&mut self, at: u64, bytes: &[u8]) -> Result<(), RunModeError> {
        let mut kept = Vec::new_in(self.alloc);
        kept.extend_from_slice(bytes);
        self.written.push((at, kept));
        self.order.push("write");
        Ok(())
    }
}

/// The simulated key.
const KEY: u64 = 0x2a;

/// What one run showed: the placement's result (or the frame's refusal), and the memory.
struct Seen {
    result: Result<(), RunModeError>,
    memory: Sim,
}

/// The entry's stub fired; then the parent's `Declare`, `Data` (when `told` holds any) and
/// `Publish` of `f`'s record, the acts carried as the native driver does, to the first error.
fn run(statics: &[(Symbol, AVec<'static, u8>)], twice: bool) -> Seen {
    run_with(code(Alloc::global()), statics, twice, 0)
}

/// [`run`] of the record `code`, with `thread_locals` `ThreadLocals` messages (naming [`DESC`]
/// and [`ENDER`]) after the `Data`.
fn run_with(code: AVec<'static, u8>, statics: &[(Symbol, AVec<'static, u8>)], twice: bool, thread_locals: usize) -> Seen {
    let pairs = [Declared { symbol: WireSymbol(F.0), slot: F_WIRE }];
    run_declaring(code, statics, twice, thread_locals, &pairs)
}

fn run_declaring(
    code: AVec<'static, u8>,
    statics: &[(Symbol, AVec<'static, u8>)],
    twice: bool,
    thread_locals: usize,
    pairs: &[Declared],
) -> Seen {
    let alloc = Alloc::global();
    let mut store = Store { records: Vec::new_in(alloc) };
    let f = store.put(code);
    let mut told: AVec<'static, Told> = Vec::new_in(alloc);
    for (symbol, bytes) in statics {
        let mut copy = Vec::new_in(alloc);
        copy.extend_from_slice(bytes);
        told.push(Told { symbol: WireSymbol(symbol.0), record: store.put(copy) });
    }
    let mut memory = Sim {
        next: AREA,
        stubbed: Vec::new_in(alloc),
        published: Vec::new_in(alloc),
        copied: Vec::new_in(alloc),
        order: Vec::new_in(alloc),
        written: Vec::new_in(alloc),
        ender: None,
        data_next: DATA,
        rooms: Vec::new_in(alloc),
        alloc,
    };
    let mut mode = RunMode::new(alloc);
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc);
    let entry = mode.start(F, &mut acts).expect("the entry is numbered");
    mode.on(Event::Tick(Nanos(1)), &mut acts).expect("a tick");
    mode.on(Event::Fired { thread: Thread(1), slot: entry }, &mut acts).expect("the entry's stub fires");
    mode.sent();
    acts.clear();

    let mut down = std::vec::Vec::new();
    Down::Declare { pairs: List::of(pairs) }.encode(0, &mut down).expect("Declare encodes");
    if !told.is_empty() {
        Down::Data { data: List::of(&told) }.encode(1, &mut down).expect("Data encodes");
        if twice {
            Down::Data { data: List::of(&told[..1]) }.encode(2, &mut down).expect("Data encodes");
        }
    }
    for n in 0..thread_locals {
        Down::ThreadLocals { descriptor: WireSymbol(DESC.0), ender: WireSymbol(ENDER.0) }
            .encode(10 + n as u64, &mut down)
            .expect("ThreadLocals encodes");
    }
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
            Act::ThreadLocals => mode.make_thread_locals(&mut memory),
            Act::Hook { slot, kind } => mode.hook(slot, kind, &mut memory),
            Act::Place { slot, record } => mode.place(slot, record, &store, &mut memory, Plant::None, &mut acts),
            Act::Wake { .. } => Ok(()),
            Act::Promote { .. } => panic!("this simulation publishes no optimized record"),
            Act::Stop(stop) => panic!("the run stopped: {stop:?}"),
            Act::Native { record } => panic!("this simulation sends no native library: {record:?}"),
        };
    }
    Seen { result, memory }
}

/// The invariant: statics, writable data, vtables, and thread-local descriptors are placed and initialized with valid addresses and function words before any record that names them.
/// Steps 1 and 2: `S` (plain bytes) and `T` (its value `S`'s address, its own byte 8's, and
/// `f`'s thunk) are placed before `f`, together; `f`'s load reads `S`'s word; each word of `T`
/// holds what it names. Controls: `f` published with no `Data` is refused naming `S`; the same
/// static sent twice is refused.
/// Prints "run mode: 2 statics placed before the record naming them; its load reads S's word;
/// T holds S, itself and f; 2 controls refused".
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
    assert_eq!(m.rooms.as_slice(), &[Room::Code, Room::Code], "read-only statics lie in the code area");
    println!("run mode: 2 statics placed before the record naming them; its load reads S's word; T holds S, itself and f; 2 controls refused");
}

/// A static initialized with `[g, f]` holds both function addresses even when only `f` has
/// a body. Change the first relocation to `f`, then omit it, and check the published bytes
/// against the same pair of expected addresses each time.
#[test]
fn the_oracle_catches_a_wrong_instance_and_a_zero_function_word() {
    let alloc = Alloc::global();
    const G: Symbol = Symbol([2; 32]);
    let pairs = [
        Declared { symbol: WireSymbol(F.0), slot: F_WIRE },
        Declared { symbol: WireSymbol(G.0), slot: Slot(5) },
    ];
    let placed = |first: Option<Symbol>, declared: &[Declared]| {
        let mut words = Vec::new_in(alloc);
        if let Some(symbol) = first {
            words.push((0, Some(Target { space: Space::Func, symbol }), 0));
        }
        words.push((8, Some(Target { space: Space::Func, symbol: F }), 0));
        let array = datum(Place::ReadOnly, &[0; 16], &words, alloc);
        let seen = run_declaring(code(alloc), &[(S, array)], false, 0, declared);
        assert_eq!(seen.result, Ok(()));
        if first == Some(G) {
            assert!(seen.memory.stubbed.contains(&SlotId(1)), "g must have a stub before its first call");
        }
        let bytes = &seen.memory.published[0].2;
        let first = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let second = u64::from_le_bytes(bytes[8..].try_into().unwrap());
        [first, second]
    };

    // The entry gets local slot 0 at start, g gets local slot 1 on declaration. Only the
    // entry is published. These addresses come from the simulated reservation's layout.
    let expected = [THUNKS + 16, THUNKS];
    assert_eq!(placed(Some(G), &pairs), expected);
    // Replaying a start list with only f's code declares f alone. Filling the datum must
    // still number and stub g from its symbol.
    assert_eq!(placed(Some(G), &pairs[..1]), expected);
    let wrong = placed(Some(F), &pairs);
    assert_eq!(wrong, [THUNKS, THUNKS]);
    assert_ne!(wrong, expected);
    let zero = placed(None, &pairs);
    assert_eq!(zero, [0, THUNKS]);
    assert_ne!(zero, expected);
    println!("static fn words hold g and f; the wrong instance and a zero word are detected");
}

/// Step 3 (the Reference, "Static items": a `static mut`, or a static whose type holds an
/// `UnsafeCell`, is written by the program): a writable datum and a zeroed one are placed in the
/// writable data area, apart from the code area, and published before the record naming them
/// as a read-only one is; the writable one's bytes are its initializer's, the zeroed one's are
/// none (its room is zero as reserved), and the record's load still reads the static's word.
/// Prints "run mode: a writable and a zeroed static placed in the data area before f".
#[test]
fn a_written_static_is_placed_in_the_data_area() {
    let alloc = Alloc::global();
    let zeroed = {
        let sealed = Datum::new_in(Place::Zeroed { len: 64 }, 4, alloc).seal().expect("the zeroed datum seals");
        let mut out = Vec::new_in(alloc);
        sealed.encode(&mut out);
        out
    };
    let seen = run(&[(S, datum(Place::Writable, HELLO, &[], alloc)), (T, zeroed)], false);
    assert_eq!(seen.result, Ok(()), "the run placed both statics and f");
    let m = &seen.memory;
    assert_eq!(m.rooms.as_slice(), &[Room::Data, Room::Data], "both in the data area");
    let (s_index, s_at, s_bytes) = &m.published[0];
    let (_, t_at, t_bytes) = &m.published[1];
    assert!(*s_at >= DATA && *t_at >= DATA, "apart from the code area: {s_at:#x} {t_at:#x}");
    assert_eq!(&s_bytes[..], &HELLO[..], "S's bytes are its initializer's");
    assert!(t_bytes.is_empty(), "a zeroed static copies nothing");
    assert_eq!(*t_at % 16, 0, "T is aligned as its datum says");
    assert!(*t_at >= s_at + HELLO.len() as u64, "the two do not overlap");
    assert_eq!(m.order.last().copied(), Some("store"), "f's word last: {:?}", m.order);
    let (code_at, code) = &m.copied[0];
    let first = u32::from_le_bytes(code[0..4].try_into().expect("4 bytes"));
    let second = u32::from_le_bytes(code[4..8].try_into().expect("4 bytes"));
    assert_eq!(adrp_ldr_target(first, second, *code_at), Some(m.static_word(*s_index)), "f's load reads S's word");
    println!("run mode: a writable and a zeroed static placed in the data area before f");
}

/// A vtable `V` (a `dyn Sub`'s) and its supertrait's `U`: different domains from the statics'
/// symbols in the real run (symbols.rs `vtable_symbol`), here plain distinct bytes.
const V: Symbol = Symbol([0x56; 32]);
const U: Symbol = Symbol([0x55; 32]);

/// `h`: `adrp x0, V@PAGE; ldr x0, [x0, V@PAGEOFF]; ret`: `V`'s address in `x0`, as a `&dyn`'s
/// metadata is made.
fn vtable_code(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X0, LDR_X0, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let v = r.target(Target { space: Space::Vtable, symbol: V });
    r.sites.push(Site { at: 0, kind: SiteKind::Load, target: v, addend: 0 });
    let sealed = r.seal().expect("the record seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// A vtable is a read-only datum the parent sends as `Data` before its first namer (the Reference,
/// "Trait objects": the pointer's metadata is the vtable's address): `V`'s header is drop glue
/// (here `f`), size 4, align 4, its method word `f` and its supercast word `U`'s address; the
/// namer's `Space::Vtable` load reads `V`'s word; each word holds what it names, a fn's thunk
/// (so a later tier moves no table) and `U`'s placed address. Control: `h` with no `Data` is
/// refused naming `V`.
/// Prints "run mode: vtable V placed as a datum before h; h's load reads V's word; V holds f's thunk twice and U; 1 control refused".
#[test]
fn a_vtable_is_placed_as_a_datum_before_the_record_that_names_it() {
    let alloc = Alloc::global();
    let f = Target { space: Space::Func, symbol: F };
    let mut header = [0u8; 40];
    header[8..16].copy_from_slice(&4u64.to_le_bytes());
    header[16..24].copy_from_slice(&4u64.to_le_bytes());
    let v = datum(Place::ReadOnly, &header, &[(0, Some(f), 0), (24, Some(f), 0), (32, Some(Target { space: Space::Vtable, symbol: U }), 0)], alloc);
    let u = datum(Place::ReadOnly, &header[..32], &[(24, Some(f), 0)], alloc);
    let seen = run_with(vtable_code(alloc), &[(V, v), (U, u)], false, 0);
    assert_eq!(seen.result, Ok(()), "the run placed the vtables and h");
    let m = &seen.memory;
    assert_eq!(m.published.len(), 2, "both vtables placed");
    assert_eq!(m.order.last().copied(), Some("store"), "h's word last: {:?}", m.order);
    let (v_index, _, v_bytes) = &m.published[0];
    let (_, u_at, _) = &m.published[1];
    let word = |at: usize| u64::from_le_bytes(v_bytes[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!((word(0), word(8), word(16), word(24), word(32)), (m.thunk(SlotId(0)), 4, 4, m.thunk(SlotId(0)), *u_at), "V's words");
    let (code_at, code) = &m.copied[0];
    let first = u32::from_le_bytes(code[0..4].try_into().expect("4 bytes"));
    let second = u32::from_le_bytes(code[4..8].try_into().expect("4 bytes"));
    assert_eq!(adrp_ldr_target(first, second, *code_at), Some(m.static_word(*v_index)), "h's load reads V's word");

    let bare = run_with(vtable_code(alloc), &[], false, 0);
    assert!(
        matches!(bare.result, Err(RunModeError::StaticNotTold { symbol, .. }) if symbol == V),
        "a record naming an untold vtable: {:?}",
        bare.result
    );
    println!("run mode: vtable V placed as a datum before h; h's load reads V's word; V holds f's thunk twice and U; 1 control refused");
}

/// The thread-local descriptor's symbol, the ender's, and thread-local `N` (its template is the
/// datum under the same symbol).
const DESC: Symbol = Symbol([0xd5; 32]);
const ENDER: Symbol = Symbol([0xe7; 32]);
const N: Symbol = Symbol([0x77; 32]);
const ADRP_X1: u32 = 0x9000_0001;
const LDR_X1: u32 = 0xF940_0021;

/// `g`: the access sequence's two loads, `adrp x0, DESC; ldr x0, [x0, DESC]` (the descriptor's
/// address) and `adrp x1, N; ldr x1, [x1, N]` (`N`'s index), then `ret`.
fn tls_code(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X0, LDR_X0, ADRP_X1, LDR_X1, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let desc = r.target(Target { space: Space::Static, symbol: DESC });
    let n = r.target(Target { space: Space::Tls, symbol: N });
    r.sites.push(Site { at: 0, kind: SiteKind::Load, target: desc, addend: 0 });
    r.sites.push(Site { at: 8, kind: SiteKind::Load, target: n, addend: 0 });
    let sealed = r.seal().expect("the record seals");
    let mut out = Vec::new_in(alloc);
    sealed.encode(&mut out);
    out
}

/// Step 4: the parent's `ThreadLocals` makes the descriptor (the key with the ender's thunk as
/// its destructor, then the header published as the static under DESC), and `g`'s load of `N`
/// numbers it: row 0 is `N`'s template (size, align, address, size), `count` is 1, and the
/// index word holds 0, all written before `g`'s word is stored; `g`'s loads read the
/// descriptor's word and the index word. Controls: no `ThreadLocals` is refused naming `N`; two
/// are refused; `N` with no template sent is refused naming it.
/// Prints "run mode: thread-local descriptor made, N numbered 0 from its template before g; 3
/// controls refused".
#[test]
fn a_thread_local_is_numbered_from_its_template_before_the_record_naming_it() {
    use blaze_code::tls::{CAP, DESC_BYTES, DESC_COUNT, descriptor, var_row};
    let alloc = Alloc::global();
    let template = || datum(Place::ReadOnly, &7u32.to_le_bytes(), &[], alloc);
    let seen = run_with(tls_code(alloc), &[(N, template())], false, 1);
    assert_eq!(seen.result, Ok(()), "the run made the descriptor and placed g");
    let m = &seen.memory;
    assert_eq!(m.ender, Some(m.thunk(SlotId(1))), "the key's destructor is the ender's entry thunk (slot 1, after g)");
    assert_eq!(m.published.len(), 2, "the template and the descriptor: {:?}", m.order);
    let (n_index, n_at, n_bytes) = &m.published[0];
    assert_eq!(&n_bytes[..], &7u32.to_le_bytes()[..], "N's template holds its initial value");
    let (desc_index, desc_at, header) = &m.published[1];
    assert_eq!(&header[..], &descriptor(KEY, KEY * 8, 0, CAP, desc_at + DESC_BYTES as u64)[..], "the header");
    let row_at = desc_at + DESC_BYTES as u64;
    let row = m.written.iter().find(|(at, _)| *at == row_at).expect("row 0 written");
    assert_eq!(&row.1[..], &var_row(4, 8, *n_at, 4)[..], "row 0 is N's template");
    let count = m.written.iter().find(|(at, _)| *at == desc_at + DESC_COUNT as u64).expect("count written");
    assert_eq!(&count.1[..], &1u64.to_le_bytes()[..], "one row");
    let index_word = m.written.last().expect("the index word written");
    assert_eq!(&index_word.1[..], &0u64.to_le_bytes()[..], "N's index is 0");
    assert_eq!(m.order.last().copied(), Some("store"), "g's word after every row and word: {:?}", m.order);
    let (code_at, code) = &m.copied[0];
    let inst = |i: usize| u32::from_le_bytes(code[4 * i..4 * i + 4].try_into().expect("4 bytes"));
    assert_eq!(adrp_ldr_target(inst(0), inst(1), *code_at), Some(m.static_word(*desc_index)), "g's first load reads the descriptor's word");
    assert_eq!(adrp_ldr_target(inst(2), inst(3), code_at + 8), Some(index_word.0), "g's second load reads N's index word");
    assert_ne!(n_index, desc_index);

    let untold = run_with(tls_code(alloc), &[(N, template())], false, 0);
    assert!(
        matches!(untold.result, Err(RunModeError::StaticNotTold { symbol, .. }) if symbol == DESC),
        "a record naming the descriptor before ThreadLocals: {:?}",
        untold.result
    );
    let twice = run_with(tls_code(alloc), &[(N, template())], false, 2);
    assert!(matches!(twice.result, Err(RunModeError::ThreadLocalsTwice)), "two ThreadLocals: {:?}", twice.result);
    let bare = run_with(tls_code(alloc), &[], false, 1);
    assert!(
        matches!(bare.result, Err(RunModeError::StaticNotTold { symbol, .. }) if symbol == N),
        "N with no template sent: {:?}",
        bare.result
    );
    println!("run mode: thread-local descriptor made, N numbered 0 from its template before g; 3 controls refused");
}
