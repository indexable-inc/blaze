//! The run mode in simulation (rule 43): a fake parent from an in-memory record, a memory that
//! records what a reader of the slot word would see after every step, and two threads firing
//! one stub. Each test prints the line its doc comment names.

use blaze_code::record::{CodeRecord, Site, SiteKind, Space, Symbol, Target};
use blaze_code::unwind::Frame;
use blaze_launch::wire::run::{Declared, Down, Needed, Symbol as WireSymbol, Up};
use blaze_launch::wire::{List, RecordId, split};
use blaze_launch::{Slot, Thread, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::{Nanos, SlotId};

use super::{Act, Event, Memory, Outside, Placing, Plant, RecordSource, RunMode, RunModeError, ThreadKey};
use crate::code_unwind::Step;

const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const RET: u32 = 0xD65F_03C0;

/// The simulated reservation: slot words, address words and thunks, then the code area 1 GiB on
/// (every `adrp` reaches).
const WORDS: u64 = 0x1_0000_0000;
const ADDRESS_WORDS: u64 = WORDS + 0x80_0000;
const THUNKS: u64 = WORDS + 0x100_0000;
const AREA: u64 = WORDS + 0x4000_0000;

const F: Symbol = Symbol([1; 32]);
const G: Symbol = Symbol([2; 32]);

/// `f`: `call g; ret`, with a standard frame, so it has an unwind entry to register.
fn record(alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for inst in [ADRP_X16, LDR_X16, BLR_X16, RET] {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    let g = r.target(Target { space: Space::Func, symbol: G });
    r.sites.push(Site { at: 0, kind: SiteKind::Call, target: g, addend: 0 });
    r.frame = Frame::PLAIN.word().expect("the plain frame has a word");
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

/// The memory a reader sees: per slot whether its stub is written, its word names the body,
/// and its unwind entry is registered; and each step in order.
struct Sim {
    next: u64,
    stubbed: AVec<'static, bool>,
    stored: AVec<'static, bool>,
    registered: AVec<'static, bool>,
    log: AVec<'static, (SlotId, Step)>,
    /// The points at which a thread loading the word would reach a body the unwinder cannot
    /// walk (the oracle's count; 0 is right).
    unwalkable: u32,
}

impl Sim {
    fn new(alloc: Alloc<'static>) -> Sim {
        Sim {
            next: AREA,
            stubbed: Vec::new_in(alloc),
            stored: Vec::new_in(alloc),
            registered: Vec::new_in(alloc),
            log: Vec::new_in(alloc),
            unwalkable: 0,
        }
    }

    fn grow(&mut self, slot: SlotId) {
        if self.stubbed.len() <= slot.index() {
            self.stubbed.resize(slot.index() + 1, false);
            self.stored.resize(slot.index() + 1, false);
            self.registered.resize(slot.index() + 1, false);
        }
    }
}

impl Memory for Sim {
    fn stub(&mut self, slot: SlotId) -> Result<(), RunModeError> {
        self.grow(slot);
        self.stubbed[slot.index()] = true;
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
        let slot = placing.slot;
        self.grow(slot);
        self.log.push((slot, step));
        match step {
            Step::Copy | Step::Flush => {}
            Step::Register => self.registered[slot.index()] = true,
            Step::Store => {
                // From here a thread loads the body; a panic in it walks its frame.
                if placing.entry.is_some() && !self.registered[slot.index()] {
                    self.unwalkable += 1;
                }
                self.stored[slot.index()] = true;
            }
        }
        Ok(())
    }

    fn static_word(&self, index: u32) -> u64 {
        WORDS - 8 * (u64::from(index) + 1)
    }

    fn reserve_data(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "this scenario places no static" })
    }

    fn publish_static(&mut self, _index: u32, _at: u64, _bytes: &[u8], _room: super::Room) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "this scenario places no static" })
    }

    fn thread_key(&mut self, _ender: u64) -> Result<ThreadKey, RunModeError> {
        Err(RunModeError::NotBuilt { what: "this scenario makes no thread-local key" })
    }

    fn write(&mut self, _at: u64, _bytes: &[u8]) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "this scenario writes no thread-local row" })
    }
}

/// What one run of the scenario shows.
struct Seen {
    needs: AVec<'static, (Needed, Thread)>,
    copies: usize,
    /// Each wake: the thread, its slot, and whether the word named the body then.
    woken: AVec<'static, (Thread, SlotId, bool)>,
    g_stubbed: bool,
    unwalkable: u32,
}

/// The driver's part, as the native one does it: a work list of acts.
fn carry(
    mode: &mut RunMode<'static>,
    acts: &mut AVec<'static, Act>,
    source: &One,
    memory: &mut Sim,
    plant: Plant,
    woken: &mut AVec<'static, (Thread, SlotId, bool)>,
) {
    let mut next = 0;
    while next < acts.len() {
        let act = acts[next];
        next += 1;
        match act {
            Act::Stub { slot } => memory.stub(slot).expect("a stub"),
            Act::Place { slot, record } => mode.place(slot, record, source, memory, plant, acts).expect("the record places"),
            Act::Promote { slot, record } => mode.promote(slot, record, source, memory, plant).expect("the record promotes"),
            Act::Wake { thread, slot } => woken.push((thread, slot, memory.stored.get(slot.index()).copied().unwrap_or(false))),
            Act::Stop(stop) => panic!("the run stopped: {stop:?}"),
            Act::Native { record } => panic!("this simulation's parent sends no native library: {record:?}"),
            Act::Statics => panic!("this simulation's parent sends no static"),
            Act::ThreadLocals => panic!("this simulation's parent sends no thread-local"),
            Act::Hook { slot, kind } => mode.hook(slot, kind, memory).expect("hook"),
        }
    }
    acts.clear();
}

/// Two threads fire `f`'s stub; the fake parent declares `f` as its slot 7 and publishes the
/// record, its bytes split across two reads.
fn scenario(plant: Plant) -> Seen {
    let alloc = Alloc::global();
    let bytes = record(alloc);
    let source = One { id: RecordId(blaze_blake3::hash(&bytes)), bytes };
    let mut memory = Sim::new(alloc);
    let mut mode = RunMode::new(alloc);
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc);
    let mut woken = Vec::new_in(alloc);

    let entry = mode.start(F, &mut acts).expect("the entry is numbered");
    assert_eq!(entry, SlotId(0), "the entry is slot 0");
    carry(&mut mode, &mut acts, &source, &mut memory, plant, &mut woken);
    mode.on(Event::Tick(Nanos(1)), &mut acts).expect("a tick");
    for thread in [Thread(1), Thread(2)] {
        mode.on(Event::Fired { thread, slot: entry }, &mut acts).expect("a stub fires");
        carry(&mut mode, &mut acts, &source, &mut memory, plant, &mut woken);
    }

    let mut needs = Vec::new_in(alloc);
    let mut up = mode.outbox();
    while let Some((item, took)) = split(up).expect("a frame") {
        let (_, Up::Need { need, thread }) = Up::decode(item).expect("a run-wire message") else { panic!("the outbox holds a fault report, and nothing crashed") };
        needs.push((need, thread));
        up = &up[took..];
    }
    assert!(up.is_empty(), "the outbox holds whole frames");
    mode.sent();

    let mut down = std::vec::Vec::new();
    let pairs = [Declared { symbol: WireSymbol(F.0), slot: Slot(7) }];
    Down::Declare { pairs: List::of(&pairs) }.encode(1, &mut down).expect("Declare encodes");
    Down::Publish { slot: Slot(7), tier: Tier::Zero, record: source.id }.encode(2, &mut down).expect("Publish encodes");
    let half = down.len() / 2;
    mode.on(Event::Frame(&down[..half]), &mut acts).expect("the first half");
    mode.on(Event::Frame(&down[half..]), &mut acts).expect("the second half");
    carry(&mut mode, &mut acts, &source, &mut memory, plant, &mut woken);

    let copies = memory.log.iter().filter(|&&(slot, step)| slot == entry && step == Step::Copy).count();
    let g_stubbed = memory.stubbed.get(1).copied().unwrap_or(false);
    Seen { needs, copies, woken, g_stubbed, unwalkable: memory.unwalkable }
}

/// One need however many threads fire one stub, one placement, both threads woken once and
/// only after the word names the body; the callee `g` got its slot and stub while `f` was
/// placed; and the planted bug (the word stored before the unwind entry is registered) is
/// caught by the oracle. Prints "run_mode: one need, one placement; plant StoreBeforeRegister
/// caught".
#[test]
fn one_need_one_placement_and_the_order_holds() {
    let seen = scenario(Plant::None);
    assert_eq!(seen.needs.len(), 1, "one need for two threads: {:?}", &seen.needs[..]);
    assert_eq!(seen.needs[0], (Needed::Symbol(WireSymbol(F.0)), Thread(1)), "the need names the symbol before the parent declared it");
    assert_eq!(seen.copies, 1, "one placement");
    assert_eq!(&seen.woken[..], &[(Thread(1), SlotId(0), true), (Thread(2), SlotId(0), true)], "both woken once, after the store");
    assert!(seen.g_stubbed, "the callee's stub is written before the caller's word is stored");
    assert_eq!(seen.unwalkable, 0, "the entry is registered before the word at every point");

    let planted = scenario(Plant::StoreBeforeRegister);
    assert_eq!(planted.unwalkable, 1, "the plant is caught");
    println!("run_mode: one need, one placement; plant StoreBeforeRegister caught");
}
