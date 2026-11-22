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

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use blaze_code::record::Symbol as CodeSymbol;
use blaze_export::native_dylib::write;
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
const START: u64 = 0x4000;
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

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], at: u64, value: u64) {
    let i = (at - START) as usize;
    bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
}

/// libSystem linked as its stub states (blaze-native libraries.rs: the install name is the
/// stub's).
fn lib_system() -> Linked<'static> {
    let Ok(Source::Stub(at)) = source(Platform::MacosArm64, Library { kind: LibraryKind::Dylib, name: "c" }) else { panic!("`c` is named by its stub") };
    let stub = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libSystem.B.dylib'\n...\n".to_owned();
    at.linked(Box::leak(stub.into_bytes().into_boxed_slice())).unwrap_or_else(|e| panic!("the stub of `c`: {e}"))
}

/// One C function `_c_seven` at 0x4000 (`mov w0, #7; ret`) and a second `_c_ret` at 0x4008,
/// one GOT cell bound to libSystem's `_puts` by flat lookup, the BNL2 descriptor at 0xC000
/// (one Rust word at 0xC030, one initializer cell at 0xC038 rebased to `_c_ret`, no pointer
/// rows, the names table at 0xC040 in the zero fill: the child reads no name), 16 bytes of
/// zero fill. The regions are blaze-export tests/native_dylib.rs's, which its writer takes.
fn image() -> Image<'static> {
    let a = alloc();
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, 0x4000, 12, 2),
        (Region::Const, 0x400C, 0, 0),
        (Region::EhFrame, 0x400C, 0, 3),
        (Region::UnwindInfo, 0x400C, 0, 2),
        (Region::Got, 0x8000, 8, 3),
        (Region::Data, 0xC000, 48, 3),
        (Region::Words, 0xC030, 8, 3),
        (Region::Inits, 0xC038, 8, 3),
        (Region::ThreadVars, 0xC040, 0, 3),
        (Region::ThreadData, 0xC040, 0, 0),
        (Region::ThreadZero, 0xC040, 0, 0),
        (Region::Zero, 0xC040, 16, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((0xC040 - START) as usize, 0);
    put32(&mut bytes, 0x4000, MOV_W0_7);
    put32(&mut bytes, 0x4004, RET);
    put32(&mut bytes, 0x4008, RET);
    // BNL2 (blaze-native descriptor.rs): magic, words, inits, pointers, word_at, init_at,
    // pointer_at, name_at.
    put32(&mut bytes, 0xC000, native_lib::MAGIC);
    put32(&mut bytes, 0xC004, 1);
    put32(&mut bytes, 0xC008, 1);
    put32(&mut bytes, 0xC00C, 0);
    put64(&mut bytes, 0xC010, 0xC030);
    put64(&mut bytes, 0xC018, 0xC038);
    put64(&mut bytes, 0xC020, 0xC040);
    put64(&mut bytes, 0xC028, 0xC040);
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: 0x8000, to: FixTo::Import { index: 0, addend: 0 } });
    fixups.push(Fixup { at: 0xC038, to: FixTo::rebase(0x4008) });
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    imports.push(Import { name: b"_puts", weak: false });
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    exports.push((b"___blaze_native_v1", 0xC000));
    exports.push((b"_c_ret", 0x4008));
    exports.push((b"_c_seven", 0x4000));
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    words.push(b"_rust_word");
    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
    libraries.push(lib_system());
    Image {
        platform: Platform::MacosArm64,
        regions,
        bytes,
        file_start: START,
        end: 0xC050,
        fixups,
        tls: Vec::new_in(a),
        imports,
        exports,
        words,
        inits: 1,
        descriptor_at: 0xC000,
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
    // SAFETY: word 0 of the loaded image, `base + word_at`, 8-aligned in its writable `__DATA`
    // for the handle's life (never closed); the load wrote it on this thread.
    let word = unsafe { ((loaded.base + 0xC030) as *const u64).read() };
    let slot = SlotId(1);
    assert_eq!(word, thunk_of(slot), "the Rust word holds the thunk of the slot the parent paired with it");
    let address = native_lib::bind(Some(&mut natives), LIBRARY, CodeSymbol([0x77; 32]), "c_seven").expect("`c_seven` binds through the handle");
    // SAFETY: `address` is `_c_seven` in the loaded image (`mov w0, #7; ret`), an
    // `extern "C" fn() -> i32` by its hand-laid code; the image stays mapped (never closed).
    let c_seven: extern "C" fn() -> i32 = unsafe { core::mem::transmute(address as usize) };
    assert_eq!(c_seven(), 7);
    let _ = std::fs::remove_dir_all(&folder);
    println!("native_load: `c_seven` through the library's handle returned 7; word 0 holds its slot's thunk; 1 init kept for the first thread");
}

/// The control: no library loaded (no `Native` sent), the same name is refused by type, naming
/// the code record's symbol. Prints "native_load: control: no library, `c_seven` is NoNativeLibrary".
#[test]
fn without_the_library_the_c_name_is_refused_by_name() {
    let symbol = CodeSymbol([0x77; 32]);
    let refused = native_lib::bind::<DlNatives>(None, LIBRARY, symbol, "c_seven");
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
