//! The launcher's REAL load of a program's native library (NATIVE.md 3.7; run_mode.rs `run`'s
//! `native`, native_lib.rs `DlNatives`): a tiny library record written with no compiler (W4's
//! writer, blaze-export native_dylib.rs, around a hand-laid image as its own tests lay one),
//! filed in a folder under its record's name, opened by `dlopen` through run mode's own path
//! (`Down::Native` read, `RunMode::load_native` over `DlNatives`), then a C name bound through
//! the library's handle the way a code record's call to it is (`native_lib::bind`) and called.
//!
//! - `a_c_function_of_the_loaded_library_returns_its_value`: C's `c_seven` (`mov w0, #7; ret`)
//!   is found through the handle and returns 7; the library's Rust word holds its slot's thunk.
//! - CONTROL `without_the_library_the_c_name_is_refused_by_name` (rule 23: the cause removed):
//!   the same bind with no library loaded is `RunModeError::NoNativeLibrary` naming the symbol,
//!   the typed failure a code record's call to `c_seven` meets when no `Native` was sent.
//! - `a_dlopen_refusal_names_the_unbound_symbol`: dyld's and ld.so's words for an import no
//!   library defines become `NativeError::Unresolved` naming it; other words stay `Refused`.

#![cfg(all(any(target_os = "macos", target_os = "linux"), target_arch = "aarch64"))]

use blaze_code::record::Symbol as CodeSymbol;
#[cfg(target_os = "macos")]
use blaze_export::native_dylib::write;
#[cfg(target_os = "linux")]
use blaze_export::native_so::write;
use blaze_launch::Slot;
use blaze_launch::wire::List;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::{Declared, Down, Symbol as WireSymbol};
use blaze_launch_rt::code_unwind::Step;
use blaze_launch_rt::native_lib::{self, DlNatives, NativeError, PATH_MOST};
use blaze_launch_rt::run_mode::{Act, CName, Event, Memory, Outside, Placing, Room, RunMode, RunModeError, ThreadKey};
use blaze_native::Platform;
use blaze_native::image::{FixTo, Fixup, Image, Import, Placed, Region};
use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::SlotId;

const LIBRARY: RecordId = RecordId([0x5c; 32]);
const IDENT: &str = "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c";
const ENTRY: CodeSymbol = CodeSymbol([0xe0; 32]);
const RUST_WORD: WireSymbol = WireSymbol([0xa0; 32]);
#[cfg(target_os = "macos")]
const START: u64 = 0x4000;
#[cfg(target_os = "macos")]
const GOT_AT: u64 = 0x8000;
#[cfg(target_os = "macos")]
const DATA_AT: u64 = 0xC000;
#[cfg(target_os = "linux")]
const START: u64 = 0x1_0000;
#[cfg(target_os = "linux")]
const GOT_AT: u64 = 0x2_0000;
#[cfg(target_os = "linux")]
const DATA_AT: u64 = 0x3_0000;
const WORDS_AT: u64 = DATA_AT + 0x30;
const INITS_AT: u64 = DATA_AT + 0x38;
const ZERO_AT: u64 = DATA_AT + 0x40;
const MOV_W0_7: u32 = 0x5280_00E0;
const RET: u32 = 0xd65f_03c0;

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn thunk_of(slot: SlotId) -> u64 {
    0x7000_0000 + 16 * u64::from(slot.0)
}

/// The run's slot table as the load sees it: the load only stubs slots and asks their thunks.
struct SlotsOnly;

impl Memory for SlotsOnly {
    fn stub(&mut self, _slot: SlotId) -> Result<(), RunModeError> {
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
    fn outside(&mut self, _record: RecordId, symbol: CodeSymbol, _name: &str, _weak: bool) -> Result<Outside, RunModeError> {
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

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], at: u64, value: u64) {
    let i = (at - START) as usize;
    bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
}

/// System C library linked as its platform states: macOS via libSystem's TBD stub, Linux via
/// glibc's `libc.so.6` soname.
fn lib_system() -> Linked<'static> {
    #[cfg(target_os = "macos")]
    {
        let Ok(Source::Stub(at)) = source(Platform::MacosArm64, Library { kind: LibraryKind::Dylib, name: "c" }) else {
            panic!("`c` is named by its stub")
        };
        let stub = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libSystem.B.dylib'\n...\n".to_owned();
        at.linked(Box::leak(stub.into_bytes().into_boxed_slice())).unwrap_or_else(|e| panic!("the stub of `c`: {e}"))
    }
    #[cfg(target_os = "linux")]
    {
        match source(Platform::LinuxArm64, Library { kind: LibraryKind::Dylib, name: "c" }) {
            Ok(Source::Linked(linked)) => linked,
            other => panic!("`c` is glibc's: {other:?}"),
        }
    }
}

/// One C function `c_seven` at `START` (`mov w0, #7; ret`) and a second `c_ret` at `START + 8`,
/// one GOT cell bound to libc's `puts`, the BNL2 descriptor at `DATA_AT` (one Rust word at
/// `WORDS_AT`, one initializer cell at `INITS_AT` rebased to `c_ret`, no pointer rows, the names
/// table at `ZERO_AT` in the zero fill: the child reads no name), 16 bytes of zero fill.
fn image() -> Image<'static> {
    let a = alloc();
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, START, 12, 2),
        (Region::Const, START + 12, 0, 0),
        (Region::EhFrame, START + 12, 0, 3),
        (Region::UnwindInfo, START + 12, 0, 2),
        (Region::Got, GOT_AT, 8, 3),
        (Region::Data, DATA_AT, 48, 3),
        (Region::Words, WORDS_AT, 8, 3),
        (Region::Inits, INITS_AT, 8, 3),
        (Region::ThreadVars, ZERO_AT, 0, 3),
        (Region::ThreadData, ZERO_AT, 0, 0),
        (Region::ThreadZero, ZERO_AT, 0, 0),
        (Region::Zero, ZERO_AT, 16, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((ZERO_AT - START) as usize, 0);
    put32(&mut bytes, START, MOV_W0_7);
    put32(&mut bytes, START + 4, RET);
    put32(&mut bytes, START + 8, RET);
    // BNL2 (blaze-native descriptor.rs): magic, words, inits, pointers, word_at, init_at,
    // pointer_at, name_at.
    put32(&mut bytes, DATA_AT, native_lib::MAGIC);
    put32(&mut bytes, DATA_AT + 4, 1);
    put32(&mut bytes, DATA_AT + 8, 1);
    put32(&mut bytes, DATA_AT + 12, 0);
    put64(&mut bytes, DATA_AT + 16, WORDS_AT);
    put64(&mut bytes, DATA_AT + 24, INITS_AT);
    put64(&mut bytes, DATA_AT + 32, ZERO_AT);
    put64(&mut bytes, DATA_AT + 40, ZERO_AT);
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: GOT_AT, to: FixTo::Import { index: 0, addend: 0 } });
    fixups.push(Fixup { at: INITS_AT, to: FixTo::rebase(START + 8) });
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    #[cfg(target_os = "macos")]
    imports.push(Import { name: b"_puts", weak: false });
    #[cfg(target_os = "linux")]
    imports.push(Import { name: b"puts", weak: false });
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    #[cfg(target_os = "macos")]
    {
        exports.push((b"___blaze_native_v1", DATA_AT));
        exports.push((b"_c_ret", START + 8));
        exports.push((b"_c_seven", START));
    }
    #[cfg(target_os = "linux")]
    {
        exports.push((b"__blaze_native_v1", DATA_AT));
        exports.push((b"c_ret", START + 8));
        exports.push((b"c_seven", START));
    }
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    #[cfg(target_os = "macos")]
    words.push(b"_rust_word");
    #[cfg(target_os = "linux")]
    words.push(b"rust_word");
    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
    libraries.push(lib_system());
    Image {
        #[cfg(target_os = "macos")]
        platform: Platform::MacosArm64,
        #[cfg(target_os = "linux")]
        platform: Platform::LinuxArm64,
        regions,
        bytes,
        file_start: START,
        end: ZERO_AT + 16,
        fixups,
        tls: Vec::new_in(a),
        imports,
        exports,
        words,
        inits: 1,
        descriptor_at: DATA_AT,
        libraries,
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

/// The library record filed as the parent files it: `<folder>/<the record's 64 hex>`, in a
/// folder of this test's own under the system's temporary folder.
fn filed(test: &str) -> std::path::PathBuf {
    let mut file: AVec<'static, u8> = Vec::new_in(alloc());
    if let Err(e) = write(&image(), IDENT, &mut file, alloc()) {
        panic!("the writer refused the image: {e:?}");
    }
    let folder = std::env::temp_dir().join(format!("blaze-native-load-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("{}: {e}", folder.display()));
    let mut path = [0u8; PATH_MOST];
    let len = native_lib::record_path(folder.as_os_str().as_encoded_bytes(), LIBRARY, &mut path).expect("a record path");
    let at = std::path::PathBuf::from(std::str::from_utf8(&path[..len]).expect("a UTF-8 path"));
    std::fs::write(&at, file.as_slice()).unwrap_or_else(|e| panic!("{}: {e}", at.display()));
    folder
}

/// Run mode told the parent's `Native { LIBRARY, [RUST_WORD at slot 1] }` after the entry: the
/// act it asks the driver to carry.
fn told(mode: &mut RunMode<'static>) {
    let mut acts: AVec<'static, Act> = Vec::new_in(alloc());
    mode.start(ENTRY, &mut acts).expect("the entry is numbered");
    let mut frame = std::vec::Vec::new();
    let pairs = [Declared { symbol: RUST_WORD, slot: Slot(1) }];
    Down::Native { library: LIBRARY, words: List::of(&pairs) }.encode(1, &mut frame).expect("Native encodes");
    acts.clear();
    mode.on(Event::Frame(&frame), &mut acts).expect("Native is read");
    assert!(acts.iter().any(|act| matches!(act, Act::Native { record } if *record == LIBRARY)), "the driver is asked to load it: {acts:?}");
}

/// Prints "native_load: `c_seven` through the library's handle returned 7; word 0 holds its
/// slot's thunk; 1 init kept for the first thread".
#[test]
fn a_c_function_of_the_loaded_library_returns_its_value() {
    let folder = filed("value");
    let mut mode = RunMode::new(alloc());
    told(&mut mode);
    let mut natives = DlNatives::new(folder.as_os_str().as_encoded_bytes()).expect("the folder fits");
    let loaded = match mode.load_native(&mut natives, &mut SlotsOnly) {
        Ok(loaded) => loaded,
        Err(RunModeError::Native(NativeError::Open { record })) => panic!("{}", native_lib::open_refused(record, natives.said())),
        Err(why) => panic!("the load: {why}"),
    };
    assert_eq!((loaded.record, loaded.inits), (LIBRARY, 1));
    native_lib::run_inits(loaded).expect("the rebased initializer runs cleanly");
    // SAFETY: word 0 of the loaded image, `base + word_at`, 8-aligned in its writable data
    // segment for the handle's life (never closed); the load wrote it on this thread.
    let word = unsafe { ((loaded.base + WORDS_AT) as *const u64).read() };
    let slot = SlotId(1);
    assert_eq!(word, thunk_of(slot), "the Rust word holds the thunk of the slot the parent paired with it");
    let address = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x77; 32]), "c_seven", false).expect("`c_seven` binds through the handle");
    // SAFETY: `address` is `_c_seven` in the loaded image (`mov w0, #7; ret`), an
    // `extern "C" fn() -> i32` by its hand-laid code; the image stays mapped (never closed).
    let c_seven: extern "C" fn() -> i32 = unsafe { core::mem::transmute(address as usize) };
    assert_eq!(c_seven(), 7);
    let _ = std::fs::remove_dir_all(&folder);
    println!("native_load: `c_seven` through the library's handle returned 7; word 0 holds its slot's thunk; 1 init ran cleanly");
}

#[test]
fn run_inits_invokes_initializers_and_refuses_null_cell() {
    use core::ffi::{c_char, c_int};
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

    static RAN: AtomicBool = AtomicBool::new(false);
    static GOT_ARGC: AtomicI32 = AtomicI32::new(-1);
    static GOT_ARGV: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn mark_ran(argc: c_int, argv: *const *const c_char, _envp: *const *const c_char) {
        GOT_ARGC.store(argc, Ordering::SeqCst);
        GOT_ARGV.store(argv as usize, Ordering::SeqCst);
        RAN.store(true, Ordering::SeqCst);
    }

    let valid_cells = [mark_ran as *const () as usize as u64];
    let loaded = native_lib::Loaded {
        record: LIBRARY,
        base: 0,
        inits_at: valid_cells.as_ptr() as u64,
        inits: 1,
    };
    let arg0 = c"blaze-test".as_ptr();
    let argv_arr = [arg0, core::ptr::null()];
    native_lib::run_inits_with(loaded, 1, argv_arr.as_ptr()).expect("valid init cell runs");
    assert!(RAN.load(Ordering::SeqCst), "initializer function must be invoked");
    assert_eq!(GOT_ARGC.load(Ordering::SeqCst), 1, "initializer must receive pre-captured argc");
    assert_eq!(GOT_ARGV.load(Ordering::SeqCst), argv_arr.as_ptr() as usize, "initializer must receive exact argv pointer");

    // Control: a null init cell is refused by name rather than called.
    let null_cells = [0u64];
    let bad = native_lib::Loaded {
        record: LIBRARY,
        base: 0,
        inits_at: null_cells.as_ptr() as u64,
        inits: 1,
    };
    assert_eq!(
        native_lib::run_inits(bad),
        Err(RunModeError::Native(NativeError::NullInit { record: LIBRARY, index: 0 })),
    );

    let folder = filed("preopen");
    let mut mode = RunMode::new(alloc());
    told(&mut mode);
    let mut preopened = DlNatives::open_record(folder.as_os_str().as_encoded_bytes(), LIBRARY)
        .unwrap_or_else(|e| panic!("preopen failed: {e}"));
    let pre_loaded = mode.load_native(&mut preopened, &mut SlotsOnly).unwrap_or_else(|e| panic!("load_native on preopened handle failed: {e}"));
    assert_eq!(pre_loaded.record, LIBRARY);
    let _ = std::fs::remove_dir_all(&folder);
}

/// The control: no library loaded (no `Native` sent), the same name is refused by type, naming
/// the code record's symbol. Prints "native_load: control: no library, `c_seven` is NoNativeLibrary".
#[test]
fn without_the_library_the_c_name_is_refused_by_name() {
    let symbol = CodeSymbol([0x77; 32]);
    let refused = native_lib::bind::<DlNatives>(None, LIBRARY, symbol, "c_seven", false);
    assert_eq!(refused, Err(RunModeError::NoNativeLibrary { symbol }));
    println!("native_load: control: no library, `c_seven` is NoNativeLibrary");
}

/// Prints "native_load: dlopen's words typed: Unresolved `_missing_fn` (dyld, both forms, and
/// ld.so); other words Refused".
#[test]
fn a_dlopen_refusal_names_the_unbound_symbol() {
    let unresolved = |said: &str| native_lib::open_refused(LIBRARY, said);
    let want = NativeError::Unresolved { record: LIBRARY, symbol: CName::new("_missing_fn") };
    assert_eq!(unresolved("dlopen(/h/.blaze/v2/run/records/5c5c, 0x0006): symbol not found in flat namespace '_missing_fn'"), want);
    assert_eq!(unresolved("dlopen(/h/r/5c5c, 0x0006): Symbol not found: _missing_fn\n  Referenced from: /h/r/5c5c"), want);
    let linux = NativeError::Unresolved { record: LIBRARY, symbol: CName::new("missing_fn") };
    assert_eq!(unresolved("/h/r/5c5c: undefined symbol: missing_fn"), linux);
    assert_eq!(
        unresolved("dlopen(/h/r/5c5c, 0x0006): tried: '/h/r/5c5c' (not a mach-o file)"),
        NativeError::Refused { record: LIBRARY, said: native_lib::Said::new("tried: '/h/r/5c5c' (not a mach-o file)") }
    );
    // dyld's real refusal is longer than a C name's 96 bytes: its cause is printed whole.
    let path = "/Users/someone/.blaze/v2/run/records/80b46309f3e6d1c2a4b5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b";
    let long = format!("dlopen({path}, 0x0006): tried: '{path}' (missing code signature in <454250A3-0000-0000-0000-000000000000> '{path}')");
    let shown = format!("{}", native_lib::open_refused(LIBRARY, &long));
    assert!(shown.contains("missing code signature"), "the refusal's cause is cut: {shown}");
    println!("native_load: dlopen's words typed: Unresolved `_missing_fn` (dyld, both forms, and ld.so); other words Refused");
}
