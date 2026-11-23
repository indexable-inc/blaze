//! Exported files that hold a native image, run on this Mac (NATIVE.md W9; main's rulings of
//! 2026-11-16). These ride blaze-export's example path (examples/export_smoke.rs,
//! tests/eh_out_run.rs), not the `blaze export` command: the Rust side is code records written
//! here, the native side is C++ compiled by the pinned clang++ on blaze's shelf, read, resolved
//! and laid by blaze-native (`resolve::plan`, `layout::lay`), and both go into one `ProgramIn`
//! (`natives`, `exports`); `export` writes the file, the test marks it executable, runs it with a
//! deadline and reads its exit status.
//!
//! - `cxx_throw_is_caught_inside_the_native_code`: Rust `main` calls `native_throw_catch(42)`;
//!   C++ throws `int` from a second function and catches it, returning the caught value; `main`
//!   exits with it. libc++abi's `__cxa_throw` walks the C++ frames by the image's rows in the
//!   file's one `__unwind_info` and calls `__gxx_personality_v0` (named through the image's GOT
//!   cell) with the image's LSDA. A missing or wrong row ends in `std::terminate` (SIGABRT).
//! - `rust_catch_over_a_panicking_c_callback`: the record form of `catch_unwind` around an
//!   `extern "C-unwind"` callback: Rust `main` calls C's `c_calls_back`, which calls the Rust
//!   word `rust_panics`, which raises (`_Unwind_RaiseException`, as std's panic does). The
//!   system unwinder walks out of `rust_panics` (a Rust row), out of `c_calls_back` (the image's
//!   row, moved by the shift) and lands in `main`'s landing pad (a Rust row with an LSDA read by
//!   the personality record, as tests/eh_out_run.rs's): `main` exits 42.
//! - `control_without_the_image_rows_the_panic_does_not_land`: the same program with the
//!   image's unwind rows, frame entries and eh_frame bytes removed (rule 23: remove the cause).
//!   The unwinder finds nothing for `c_calls_back`, `_Unwind_RaiseException` returns, and
//!   `rust_panics` reaches its `brk` (SIGTRAP): anything but 42.
//!
//! Ignored: they need the pinned C toolchain unpacked on the shelf
//! (`$HOME/.blaze/v2/toolchain/clang/<sha256>`, the digest from
//! `blaze_pins::CLANG_JSON`) and run the exported files. The integrator runs them by
//! name on an arm64 Mac: `cargo test -p blaze-export --test native_run -- --ignored --nocapture`.
//!
//! The test is a driver at the edge: it names the clock, the file system, `HOME` and the process
//! launcher through `std`, as export_smoke does (bstd has no public spawn-and-wait with a
//! deadline yet). Every wait has a deadline (rule 18): clang 60 s, each run 20 s.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use blaze_build_script::toolchain::clang_pin;
use blaze_pins::CLANG_JSON as PIN;
use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Symbol, Target};
use blaze_code::unwind::{self, Action, Frame, Row};
use blaze_export::{CExport, Entry, Function, Panic, Platform, ProgramIn, Tier, export};
use blaze_hash::FixedState;
use blaze_native::Platform as NativePlatform;
use blaze_native::image::{Image, Region};
use blaze_native::layout::{Layout, lay};
use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
use blaze_native::resolve::{Input, InputKind, Origin, Wanted, plan};
use blaze_own::{AVec, Alloc, Vec};

const STP: u32 = 0xA9BF_7BFD; // stp x29, x30, [sp, #-16]!
const MOV_FP: u32 = 0x9100_03FD; // mov x29, sp
const ADRP_X16: u32 = 0x9000_0010;
const LDR_X16: u32 = 0xF940_0210;
const BLR_X16: u32 = 0xD63F_0200;
const RET: u32 = 0xD65F_03C0;
const MOV_W0_1: u32 = 0x5280_0020;
const MOV_W0_42: u32 = 0x5280_0540;

/// The host's row in the pin table (pins/clang.json).
const HOST: &str = "aarch64-apple-darwin";
const CLANG_DEADLINE: Duration = Duration::from_secs(60);
const RUN_DEADLINE: Duration = Duration::from_secs(20);

/// Test 1's native code. No header: `-c` needs no SDK.
const THROW_CATCH: &str = r#"extern "C" __attribute__((noinline)) void native_throw(int value) { throw value; }

extern "C" int native_throw_catch(int value) {
    try {
        native_throw(value);
    } catch (int caught) {
        return caught;
    }
    return 1;
}
"#;
const THROW_CATCH_ROOTS: &[&[u8]] = &[b"_native_throw_catch"];
/// `__cxa_throw`, `__gxx_personality_v0` and `typeinfo for int` are libc++abi's, which
/// libc++.1.dylib re-exports.
/// `-lc++` linked as the macOS 27.2 SDK's `usr/lib/libc++.tbd` states it (blaze-native
/// libraries.rs: a macOS library's install name is its stub's, read).
fn throw_catch_libraries() -> &'static [Linked<'static>] {
    let Ok(Source::Stub(at)) = source(NativePlatform::MacosArm64, Library { kind: LibraryKind::Dylib, name: "c++" }) else { panic!("`c++` is named by its stub") };
    let stub: &'static [u8] = b"--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libc++.1.dylib'\n...\n";
    let linked = at.linked(stub).unwrap_or_else(|e| panic!("the stub of `c++`: {e}"));
    Box::leak(Box::new([linked]))
}

/// Test 2's native code: the callback's caller keeps a frame (the `return 1` after the call is
/// no tail call), so the unwinder must walk through a C frame.
const CALLS_BACK: &str = r#"extern "C" void rust_panics(void);

extern "C" __attribute__((noinline)) int c_calls_back(void) {
    rust_panics();
    return 1;
}
"#;
const CALLS_BACK_ROOTS: &[&[u8]] = &[b"_c_calls_back"];
const CALLS_BACK_RUST: &[&[u8]] = &[b"_rust_panics"];

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn sym(name: &str) -> Symbol {
    Symbol::of_name(name)
}

fn rec(code: &[u32], sites: &[(u32, SiteKind, Target)], outside: &[(u32, &str)], frame: Option<Frame>, rows: &[Row]) -> AVec<'static, u8> {
    let a = alloc();
    let mut r = CodeRecord::new_in(a);
    for &inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, kind, target) in sites {
        let index = r.target(target);
        r.sites.push(Site { at, kind, target: index, addend: 0 });
    }
    for &(at, name) in outside {
        let index = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    if let Some(frame) = frame {
        unwind::fill(&mut r, frame, rows).unwrap();
    }
    let mut out = Vec::new_in(a);
    r.seal().unwrap().encode(&mut out);
    out
}

/// A fresh folder of this process for one test's files.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-export-native-run-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    dir
}

/// Waits for `child` until `deadline`; past it the child is killed and the test fails naming
/// `what` (a hang is a bug, rule 69).
fn wait(mut child: Child, what: &str, deadline: Duration) -> ExitStatus {
    let until = Instant::now() + deadline;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > until {
            child.kill().unwrap();
            panic!("{what} did not exit within {} s: a hang is a bug", deadline.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The pinned clang++ on blaze's shelf: `$HOME/.blaze/v2/toolchain/clang/<sha256 hex>/bin/clang++`.
fn clangxx() -> PathBuf {
    let pin = clang_pin(PIN, HOST).unwrap_or_else(|e| panic!("{e}"));
    let hex: std::string::String = pin.sha256.iter().map(|b| format!("{b:02x}")).collect();
    let home = std::env::var_os("HOME").expect("HOME names the home the shelf is under");
    let path = Path::new(&home).join(".blaze/v2/toolchain/clang").join(hex).join("bin/clang++");
    assert!(
        path.is_file(),
        "{}: the pinned C toolchain is not unpacked on the shelf (D69: `cargo run -p blaze-proc-macro --example clang_shelf -- $HOME/.blaze/v2/toolchain/clang`, then unpack)",
        path.display()
    );
    path
}

/// `source` compiled by the pinned clang++ to one arm64 Mach-O object: its bytes.
fn compile(source: &str, name: &str, dir: &Path) -> std::vec::Vec<u8> {
    let cpp = dir.join(format!("{name}.cpp"));
    let object = dir.join(format!("{name}.o"));
    std::fs::write(&cpp, source).unwrap_or_else(|e| panic!("{}: {e}", cpp.display()));
    let child = Command::new(clangxx())
        .args(["-c", "-O1", "-std=c++17", "-fexceptions", "-target", "arm64-apple-macos11", "-o"])
        .arg(&object)
        .arg(&cpp)
        .spawn()
        .unwrap_or_else(|e| panic!("the pinned clang++: {e}"));
    let status = wait(child, "the pinned clang++", CLANG_DEADLINE);
    assert!(status.success(), "the pinned clang++ refused {}: {status}", cpp.display());
    std::fs::read(&object).unwrap_or_else(|e| panic!("{}: {e}", object.display()))
}

/// The native image of one object: resolved from `wanted` and laid for macOS arm64, as the run
/// path's native library record holds it.
fn native<'a>(object: &'a [u8], file: &'a str, wanted: Wanted<'a>) -> Image<'a> {
    let a = alloc();
    let inputs = [Input { from: Origin { package: "native-run", file }, bytes: object, kind: InputKind::Object }];
    let planned = plan(NativePlatform::MacosArm64, &inputs, wanted, FixedState, a).unwrap_or_else(|e| panic!("resolve {file}: {e:?}"));
    lay(&planned, &inputs, Layout::MACOS_ARM64, a).unwrap_or_else(|e| panic!("lay {file}: {e:?}"))
}

/// `program` exported for this Mac, written to `dir`, marked executable and run: its status.
fn export_and_run(program: &ProgramIn<'_>, dir: &Path) -> ExitStatus {
    let a = alloc();
    let mut file = Vec::new_in(a);
    export(program, Platform::MacosArm64, &mut file, a).unwrap_or_else(|e| panic!("export {}: {e}", program.name));
    let path = dir.join(program.name);
    std::fs::write(&path, &file[..]).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let child = Command::new(&path).spawn().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    wait(child, &format!("the exported program at {}", path.display()), RUN_DEADLINE)
}

/// Prints `native_run: the C++ throw was caught inside the native code and main exited 42`.
#[test]
#[ignore = "needs the pinned clang on the shelf and runs the exported file; the integrator runs it by name on an arm64 Mac"]
fn cxx_throw_is_caught_inside_the_native_code() {
    let dir = scratch("throw-catch");
    let object = compile(THROW_CATCH, "throw_catch", &dir);
    let image = native(&object, "throw_catch.o", Wanted { roots: THROW_CATCH_ROOTS, rust: &[], libraries: throw_catch_libraries() });
    // main: w0 = 42; w0 = native_throw_catch(w0); exit(w0).
    let main = rec(
        &[
            STP, MOV_FP,    // 0, 4
            MOV_W0_42,      // 8
            ADRP_X16, LDR_X16, BLR_X16, // 12: native_throw_catch
            ADRP_X16, LDR_X16, BLR_X16, // 24: exit
        ],
        &[],
        &[(12, "native_throw_catch"), (24, "exit")],
        Some(Frame::PLAIN),
        &[],
    );
    let functions = [Function { symbol: sym("main"), record: CodeView::read(&main).unwrap(), tier: Tier::Fast }];
    let program = ProgramIn {
        name: "native-throw-catch",
        functions: &functions,
        data: &[],
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: None,
        profile: None,
        natives: Some(&image),
        exports: &[],
    };
    let status = export_and_run(&program, &dir);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(status.code(), Some(42), "the exported program ended with {status}, not by the C++ catch's value");
    println!("native_run: the C++ throw was caught inside the native code and main exited 42");
}

/// main (calls C's `c_calls_back`; its row lands at 28, which exits 42; the normal path exits
/// 1), `rust_panics` (raises a zeroed `_Unwind_Exception`, `brk` if the raise returns) and the
/// personality routine (tests/eh_out_run.rs's: search phase answers "handler found", cleanup
/// phase installs the first call site's landing).
fn records() -> [AVec<'static, u8>; 3] {
    let main = rec(
        &[
            STP, MOV_FP, // 0, 4
            ADRP_X16, LDR_X16, BLR_X16, // 8: c_calls_back
            MOV_W0_1,    // 20
            0x1400_0002, // 24: b 32
            MOV_W0_42,   // 28: the landing
            ADRP_X16, LDR_X16, BLR_X16, // 32: exit
        ],
        &[],
        &[(8, "c_calls_back"), (32, "exit")],
        Some(Frame::PLAIN),
        &[Row { start: 8, len: 12, landing: 28, action: Action::Cleanup }],
    );
    let rust_panics = rec(
        &[
            STP, MOV_FP,     // 0, 4
            0xD100_83FF, // 8: sub sp, sp, #32
            0xA900_7FFF, // 12: stp xzr, xzr, [sp]
            0xA901_7FFF, // 16: stp xzr, xzr, [sp, #16]
            0x9100_03E0, // 20: mov x0, sp
            ADRP_X16, LDR_X16, BLR_X16, // 24: _Unwind_RaiseException
            0xD420_0000, // 36: brk #0
        ],
        &[],
        &[(24, "_Unwind_RaiseException")],
        Some(Frame::PLAIN),
        &[],
    );
    let personality = rec(
        &[
            0x1200_0029, // 0: and w9, w1, #1 (_UA_SEARCH_PHASE)
            0x3500_02E9, // 4: cbnz w9, 96
            0xA9BE_7BFD, // 8: stp x29, x30, [sp, #-32]!
            0x9100_03FD, // 12: mov x29, sp
            0xA901_53F3, // 16: stp x19, x20, [sp, #16]
            0xAA04_03F3, // 20: mov x19, x4 (the context)
            0xAA04_03E0, // 24: mov x0, x4
            ADRP_X16, LDR_X16, BLR_X16, // 28: _Unwind_GetLanguageSpecificData
            0xB940_0C14, // 40: ldr w20, [x0, #12] (cs_lpad of the first call site)
            0xAA13_03E0, // 44: mov x0, x19
            ADRP_X16, LDR_X16, BLR_X16, // 48: _Unwind_GetRegionStart
            0x8B14_0001, // 60: add x1, x0, x20
            0xAA13_03E0, // 64: mov x0, x19
            ADRP_X16, LDR_X16, BLR_X16, // 68: _Unwind_SetIP
            0x5280_00E0, // 80: mov w0, #7 (_URC_INSTALL_CONTEXT)
            0xA941_53F3, // 84: ldp x19, x20, [sp, #16]
            0xA8C2_7BFD, // 88: ldp x29, x30, [sp], #32
            RET,         // 92
            0x5280_00C0, // 96: mov w0, #6 (_URC_HANDLER_FOUND)
            RET,         // 100
        ],
        &[],
        &[(28, "_Unwind_GetLanguageSpecificData"), (48, "_Unwind_GetRegionStart"), (68, "_Unwind_SetIP")],
        None,
        &[],
    );
    [main, rust_panics, personality]
}

/// Removes every unwind fact of `image`: its rows, its frame entries, the pointers inside them,
/// and its eh_frame's bytes (a zero-length region: nothing is copied into the file's one
/// eh_frame, so no stale FDE can be found by a linear search either).
fn drop_unwind(image: &mut Image<'_>) {
    image.unwind.clear();
    image.frames.clear();
    image.eh_pointers.clear();
    for placed in image.regions.iter_mut() {
        if placed.region == Region::EhFrame {
            placed.len = 0;
        }
    }
}

/// Exports and runs the callback program; `without_rows` removes the image's unwind facts.
fn run_callback_program(name: &str, without_rows: bool) -> ExitStatus {
    let dir = scratch(name);
    let object = compile(CALLS_BACK, "calls_back", &dir);
    let mut image = native(&object, "calls_back.o", Wanted { roots: CALLS_BACK_ROOTS, rust: CALLS_BACK_RUST, libraries: &[] });
    if without_rows {
        drop_unwind(&mut image);
    }
    let records = records();
    let names = ["main", "rust_panics", "personality"];
    let functions: [Function<'_>; 3] = [0, 1, 2].map(|k| Function { symbol: sym(names[k]), record: CodeView::read(&records[k]).unwrap(), tier: Tier::Fast });
    let exports = [CExport { name: b"rust_panics", symbol: sym("rust_panics") }];
    let program = ProgramIn {
        name,
        functions: &functions,
        data: &[],
        entry: Entry::CMain { main: sym("main") },
        panic: Panic::Unwind,
        personality: Some(sym("personality")),
        tls: None,
        profile: None,
        natives: Some(&image),
        exports: &exports,
    };
    let status = export_and_run(&program, &dir);
    std::fs::remove_dir_all(&dir).unwrap();
    status
}

/// Prints `native_run: the panic in rust_panics unwound through c_calls_back into main's landing and exited 42`.
#[test]
#[ignore = "needs the pinned clang on the shelf and runs the exported file; the integrator runs it by name on an arm64 Mac"]
fn rust_catch_over_a_panicking_c_callback() {
    let status = run_callback_program("native-catch-over-callback", false);
    assert_eq!(status.code(), Some(42), "the exported program ended with {status}, not by main's landing");
    println!("native_run: the panic in rust_panics unwound through c_calls_back into main's landing and exited 42");
}

/// Prints `native_run control: without the image's unwind rows the panic did not land (signal: 5 (SIGTRAP))`.
#[test]
#[ignore = "needs the pinned clang on the shelf and runs the exported file; the integrator runs it by name on an arm64 Mac"]
fn control_without_the_image_rows_the_panic_does_not_land() {
    let status = run_callback_program("native-catch-control", true);
    assert_ne!(status.code(), Some(42), "control: the panic landed with no unwind facts for c_calls_back: the test does not test the image's rows");
    println!("native_run control: without the image's unwind rows the panic did not land ({status})");
}
