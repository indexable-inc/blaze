//! The launcher's load of the program's native library, sans IO (port27/tempo-run/NATIVE.md
//! 3.4 and 3.7; piece W8): a fake `Natives` stands for `dlopen`/`dlsym`, a fake `Memory` for the
//! run's slot table. No dylib is made or loaded here (W11's rung loads a real one).
//!
//! The rules tested, from NATIVE.md 3.4/3.7 and the coordinator's ruling of 2026-11-16 (BNL2):
//! "The launcher fills word i with the entry thunk of the slot the parent declared for name i";
//! a C data cell holding a Rust function's address gets that function's one address, its
//! slot's thunk; "`words` equals the list's length"; "An `outside()` before the library is
//! loaded: `RunModeError::NoNativeLibrary { symbol }`"; BNL1 is an old record, refused by tag.

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::Slot;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::{Declared, Down, Symbol as WireSymbol};
use blaze_launch::wire::List;
use blaze_launch_rt::code_unwind::Step;
use blaze_launch_rt::native_lib::{
    self, DESCRIPTOR_LEN, Descriptor, Loaded, MAGIC, MAGIC_BNL1, NativeError, Natives, PATH_MOST, POINTER_ROW_LEN,
    PointerRow,
};
use blaze_launch_rt::run_mode::{Act, Event, Memory, Outside, Placing, Room, RunMode, RunModeError, ThreadKey};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::SlotId;

const BASE: u64 = 0x1_0000_0000;
const LIBRARY: RecordId = RecordId([0xab; 32]);
const ENTRY: CodeSymbol = CodeSymbol([0xe0; 32]);
const A: WireSymbol = WireSymbol([0xa0; 32]);
const B: WireSymbol = WireSymbol([0xb0; 32]);

/// The thunk address the fake memory gives slot `slot`.
fn thunk_of(slot: SlotId) -> u64 {
    0x7000_0000 + 16 * u64::from(slot.0)
}

/// The run's slot table as a reader sees it: which slots are stubbed.
struct FakeMemory {
    stubbed: AVec<'static, bool>,
}

impl Memory for FakeMemory {
    fn stub(&mut self, slot: SlotId) -> Result<(), RunModeError> {
        if self.stubbed.len() <= slot.index() {
            self.stubbed.resize(slot.index() + 1, false);
        }
        self.stubbed[slot.index()] = true;
        Ok(())
    }
    fn word(&self, slot: SlotId) -> u64 {
        0x6000_0000 + 8 * u64::from(slot.0)
    }
    fn address_word(&self, slot: SlotId) -> u64 {
        0x6800_0000 + 8 * u64::from(slot.0)
    }
    fn thunk(&self, slot: SlotId) -> u64 {
        thunk_of(slot)
    }
    fn outside(&mut self, _record: RecordId, symbol: CodeSymbol, _name: &str) -> Result<Outside, RunModeError> {
        Err(RunModeError::NoNativeLibrary { symbol })
    }
    fn reserve(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn reserve_hot(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn carry(&mut self, _step: Step, _placing: &Placing<'_>) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no code" })
    }
    fn static_word(&self, index: u32) -> u64 {
        0x5800_0000 + 8 * u64::from(index)
    }
    fn reserve_data(&mut self, _len: usize, _align: usize) -> Result<u64, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no static" })
    }
    fn publish_static(&mut self, _index: u32, _at: u64, _bytes: &[u8], _room: Room) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests place no static" })
    }
    fn thread_key(&mut self, _ender: u64) -> Result<ThreadKey, RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests make no thread-local key" })
    }
    fn write(&mut self, _at: u64, _bytes: &[u8]) -> Result<(), RunModeError> {
        Err(RunModeError::NotBuilt { what: "these tests write no thread-local row" })
    }
}

/// `dlopen`/`dlsym` as a table: one image at [`BASE`] with `descriptor` and `rows`; every
/// cell written, in order; the C names it defines.
struct FakeNatives {
    descriptor: Descriptor,
    rows: &'static [PointerRow],
    opened: u32,
    writes: AVec<'static, (u64, u64)>,
    names: &'static [(&'static str, u64)],
}

impl Natives for FakeNatives {
    fn open(&mut self, record: RecordId) -> Result<u64, NativeError> {
        assert_eq!(record, LIBRARY, "the record the parent named is opened");
        self.opened += 1;
        Ok(BASE)
    }
    fn descriptor(&mut self, _record: RecordId) -> Result<Descriptor, NativeError> {
        Ok(self.descriptor)
    }
    fn pointer_row(&mut self, address: u64) -> PointerRow {
        let row = (address - BASE - self.descriptor.pointer_at) / POINTER_ROW_LEN;
        self.rows[row as usize]
    }
    fn write_word(&mut self, address: u64, value: u64) {
        self.writes.push((address, value));
    }
    fn lookup(&mut self, name: &str) -> Option<u64> {
        self.names.iter().find(|(it, _)| *it == name).map(|&(_, at)| at)
    }
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn descriptor(words: u32, pointers: u32) -> Descriptor {
    Descriptor { magic: MAGIC, words, inits: 1, pointers, word_at: 0x8000, init_at: 0x8010, pointer_at: 0x8100, name_at: 0x8030 }
}

fn fake(descriptor: Descriptor, rows: &'static [PointerRow]) -> FakeNatives {
    FakeNatives { descriptor, rows, opened: 0, writes: Vec::new_in(alloc()), names: &[("write", 0x1234_5678)] }
}

/// A run mode with the entry numbered (slot 0) and the parent's `Native { LIBRARY, pairs }`
/// read: the acts it gave.
fn declared(mode: &mut RunMode<'static>, pairs: &[Declared]) -> AVec<'static, Act> {
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc());
    mode.start(ENTRY, &mut acts).expect("the entry is numbered");
    let mut frame = std::vec::Vec::new();
    Down::Native { library: LIBRARY, words: List::of(pairs) }.encode(1, &mut frame).expect("Native encodes");
    mode.on(Event::Frame(&frame), &mut acts).expect("Native is read");
    acts
}

/// Word i holds the thunk of the parent's pair i's slot, the pointer row's cell the thunk of its
/// word's slot (one address per function), every declared slot is stubbed, the init list is
/// handed back for the first thread, and a second `Native` is refused. Prints "native_lib:
/// 2 words and 1 pointer cell hold their slots' thunks; 1 init kept for the first thread;
/// a second Native refused (Twice)".
#[test]
fn fills_words_with_thunks() {
    let mut mode = RunMode::new(alloc());
    let pairs = [Declared { symbol: A, slot: Slot(5) }, Declared { symbol: B, slot: Slot(9) }];
    let acts = declared(&mut mode, &pairs);
    assert_eq!(acts.last(), Some(&Act::Native { record: LIBRARY }), "the load is asked of the driver: {acts:?}");
    static ROWS: [PointerRow; 1] = [PointerRow { at: 0x9000, word: 1, pad: 0 }];
    let mut natives = fake(descriptor(2, 1), &ROWS);
    let mut memory = FakeMemory { stubbed: Vec::new_in(alloc()) };
    let loaded = mode.load_native(&mut natives, &mut memory).expect("the library loads");
    // The entry is local slot 0; A and B are numbered next, in the pairs' order.
    let (a, b) = (SlotId(1), SlotId(2));
    assert!(memory.stubbed.get(1) == Some(&true) && memory.stubbed.get(2) == Some(&true), "both slots stubbed");
    assert_eq!(
        natives.writes.as_slice(),
        [(BASE + 0x8000, thunk_of(a)), (BASE + 0x8008, thunk_of(b)), (BASE + 0x9000, thunk_of(b))],
        "word i = thunk(slot_i); the pointer cell = thunk of its word's slot"
    );
    assert_eq!(loaded, Loaded { record: LIBRARY, base: BASE, inits_at: BASE + 0x8010, inits: 1 });
    assert_eq!(mode.native(), Some(loaded));
    assert_eq!(natives.opened, 1);

    let mut again = std::vec::Vec::new();
    Down::Native { library: LIBRARY, words: List::of(&pairs) }.encode(2, &mut again).expect("Native encodes");
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc());
    let twice = mode.on(Event::Frame(&again), &mut acts);
    assert_eq!(twice, Err(RunModeError::Native(NativeError::Twice { record: LIBRARY })));
    println!(
        "native_lib: {} words and {} pointer cell hold their slots' thunks; {} init kept for the first thread; a second Native refused (Twice)",
        2, 1, loaded.inits
    );
}

/// The image's word count must equal the parent's pairs; a BNL1 descriptor is an old record,
/// refused by its tag, never read beside BNL2; a pointer row naming a word past the count is
/// refused before any pointer cell is written. Prints "native_lib: Words { .., image: 2,
/// declared: 1 }; Old { tag: 0x424e4c31 }; PointerWord { row: 0, word: 3, words: 1 }".
#[test]
fn refuses_word_count_mismatch() {
    let pairs = [Declared { symbol: A, slot: Slot(5) }];

    let mut mode = RunMode::new(alloc());
    declared(&mut mode, &pairs);
    let mut natives = fake(descriptor(2, 0), &[]);
    let mut memory = FakeMemory { stubbed: Vec::new_in(alloc()) };
    let got = mode.load_native(&mut natives, &mut memory);
    let Err(RunModeError::Native(NativeError::Words { image, declared: count, .. })) = got else {
        panic!("a word count mismatch was not refused: {got:?}")
    };
    assert_eq!((image, count), (2, 1));
    assert!(natives.writes.is_empty(), "nothing written before the refusal");

    let mut mode = RunMode::new(alloc());
    declared(&mut mode, &pairs);
    let mut old = fake(Descriptor { magic: MAGIC_BNL1, ..descriptor(1, 0) }, &[]);
    let got = mode.load_native(&mut old, &mut memory);
    assert_eq!(got, Err(RunModeError::Native(NativeError::Old { record: LIBRARY, tag: MAGIC_BNL1 })));

    let mut mode = RunMode::new(alloc());
    declared(&mut mode, &pairs);
    static PAST: [PointerRow; 1] = [PointerRow { at: 0x9000, word: 3, pad: 0 }];
    let mut past = fake(descriptor(1, 1), &PAST);
    let got = mode.load_native(&mut past, &mut memory);
    assert_eq!(got, Err(RunModeError::Native(NativeError::PointerWord { record: LIBRARY, row: 0, word: 3, words: 1 })));
    assert_eq!(past.writes.len(), 1, "the word only: no pointer cell written for a refused row");
    assert_eq!(DESCRIPTOR_LEN, core::mem::size_of::<Descriptor>() as u64, "BNL2 is 48 bytes");
    assert_eq!(POINTER_ROW_LEN, core::mem::size_of::<PointerRow>() as u64);
    println!(
        "native_lib: Words {{ .., image: {image}, declared: {count} }}; Old {{ tag: {MAGIC_BNL1:#x} }}; PointerWord {{ row: 0, word: 3, words: 1 }}"
    );
}

/// A C name before the library is loaded is `NoNativeLibrary` naming the symbol; after, a name
/// the image and its libraries lack is `Outside`, one they have is its address. Prints
/// "native_lib: NoNativeLibrary before the library; `write` at 0x12345678 after; a name it
/// lacks is Outside".
#[test]
fn outside_before_native_refused() {
    let symbol = CodeSymbol([0x33; 32]);
    let before = native_lib::bind::<FakeNatives>(None, LIBRARY, symbol, "write");
    assert_eq!(before, Err(RunModeError::NoNativeLibrary { symbol }));
    let mut natives = fake(descriptor(0, 0), &[]);
    let found = native_lib::bind(Some(&mut natives), LIBRARY, symbol, "write");
    assert_eq!(found, Ok(0x1234_5678));
    let lacks = native_lib::bind(Some(&mut natives), LIBRARY, symbol, "not_there");
    assert!(matches!(lacks, Err(RunModeError::Outside { record: LIBRARY, .. })), "the refusal names the record: {lacks:?}");
    let nul = native_lib::bind(Some(&mut natives), LIBRARY, symbol, "a\0b");
    assert!(matches!(nul, Err(RunModeError::Outside { .. })), "{nul:?}");
    println!("native_lib: NoNativeLibrary before the library; `write` at {:#x} after; a name it lacks is Outside", 0x1234_5678);
}

/// A record's file is `<folder>/<64 lowercase hex>` NUL-terminated; a folder too long is
/// refused. Prints "native_lib: record path /s/abab..abab (67 bytes)".
#[test]
fn record_path_is_folder_slash_hex() {
    let mut out = [0u8; PATH_MOST];
    let len = native_lib::record_path(b"/s", LIBRARY, &mut out).expect("a path");
    assert_eq!(len, 3 + 64);
    assert_eq!(&out[..3], b"/s/");
    assert!(out[3..len].chunks(2).all(|it| it == b"ab"));
    assert_eq!(out[len], 0);
    let long = [b'x'; PATH_MOST];
    assert!(matches!(native_lib::record_path(&long, LIBRARY, &mut out), Err(RunModeError::StorePath { .. })));
    println!("native_lib: record path /s/abab..abab ({len} bytes)");
}

/// The symbols run_inits reads the process's arguments through exist and answer (rule 45's
/// check, though they are in the SDK's public <crt_externs.h>). Prints "native_lib:
/// _NSGetArgc/_NSGetArgv/_NSGetEnviron answer (argc >= 1)".
#[cfg(target_os = "macos")]
#[test]
fn ns_get_args_exist() {
    // SAFETY: the three return pointers to the process's own argc/argv/environ, set before
    // any code ran.
    let (argc, argv, envp) = unsafe { (*libc::_NSGetArgc(), *libc::_NSGetArgv(), *libc::_NSGetEnviron()) };
    assert!(argc >= 1 && !argv.is_null() && !envp.is_null());
    println!("native_lib: _NSGetArgc/_NSGetArgv/_NSGetEnviron answer (argc >= 1)");
}
