//! D3's tests (port27/WAVE-BACKEND.md): LLVM's object read back into the new code record
//! (blaze-llvm read.rs), sealed, placed by blaze-tier's `place` and run.
//!
//! The oracle of the run is the function's meaning, written beside its IR: `d3_f(a, b) =
//! 3 * d3_outside(a + b + TABLE[a & 3])` with `d3_outside(x) = 7x + 1`, all in wrapping 64-bit
//! arithmetic (LLVM IR `add`/`mul` without `nsw`/`nuw` wrap: LangRef, "add Instruction": "the
//! result value of the add is the integer result modulo 2^n"). A relocation read to the wrong
//! target, kind or addend (the call reaching elsewhere, the table's page or offset wrong) runs to
//! another value or faults, so the comparison fails. Each test's doc says the line it prints.

#![feature(allocator_api)]

use blaze_code::image::ImageError;
use blaze_code::place::{At, Words};
use blaze_code::record::{OWN_RELRO, Space, Symbol, Target};
use blaze_code::unwind::Frame;
use blaze_llvm::read::{ReadError, read_object};
use blaze_llvm::{Pipeline, Remark, compile, init};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{encode, place, read, seal};
use blaze_tier::{Arch, Emitted, Machine, Os, RelocKind};

const MAC: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };
const MAC_CPU: &str = "apple-m1";
const LINUX: Machine = Machine { arch: Arch::Arm64, os: Os::Linux, features: Machine::LSE };
const LINUX_CPU: &str = "neoverse-n1";
const X86_LINUX: Machine = Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 };
const X86_CPU: &str = "x86-64-v3";

const TABLE: [i64; 4] = [7, 1000, 13, 99_999];

/// A function calling an outside one, reading a private constant table: nothing is live across
/// the call, so LLVM saves no callee-saved register and the frame is the plain frame record.
const F_IR: &str = "@d3_table = private unnamed_addr constant [4 x i64] [i64 7, i64 1000, i64 13, i64 99999], align 8
declare i64 @d3_outside(i64)
define i64 @d3_f(i64 %a, i64 %b) \"frame-pointer\"=\"non-leaf\" {
  %i = and i64 %a, 3
  %p = getelementptr inbounds [4 x i64], ptr @d3_table, i64 0, i64 %i
  %v = load i64, ptr %p, align 8
  %s = add i64 %a, %b
  %x = add i64 %s, %v
  %r = call i64 @d3_outside(i64 %x)
  %m = mul i64 %r, 3
  ret i64 %m
}
";

/// `%a` is live across two calls: LLVM keeps it in a callee-saved register, saved in its
/// own layout.
const G_IR: &str = "declare i64 @d3_outside(i64)
define i64 @d3_g(i64 %a) \"frame-pointer\"=\"non-leaf\" {
  %r = call i64 @d3_outside(i64 %a)
  %s = call i64 @d3_outside(i64 %r)
  %t = add i64 %s, %a
  ret i64 %t
}
";

/// A call with no frame-pointer attribute: LLVM saves `x30` alone, with no frame record, which
/// the compact encoding cannot say, so the Mach-O object's compact entry is in DWARF mode and the
/// frame is the FDE's in `__TEXT,__eh_frame`.
const H_IR: &str = "declare i64 @d3_outside(i64)
define i64 @d3_h(i64 %a) {
  %r = call i64 @d3_outside(i64 %a)
  %m = mul i64 %r, 3
  ret i64 %m
}
";

/// A thread-local's load: a relocation kind no record row expresses. With no frame-pointer
/// attribute, the Mach-O object also describes its frame in `__TEXT,__eh_frame`.
const T_IR: &str = "@d3_tls = external thread_local global i64
define i64 @d3_t() {
  %v = load i64, ptr @d3_tls, align 8
  ret i64 %v
}
";

extern "C" fn d3_outside(x: i64) -> i64 {
    x.wrapping_mul(7).wrapping_add(1)
}

fn expected(a: i64, b: i64) -> i64 {
    let x = a.wrapping_add(b).wrapping_add(TABLE[(a & 3) as usize]);
    d3_outside(x).wrapping_mul(3)
}

/// The test's root allocator capability (D102).
fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn outside() -> Target {
    Target { space: Space::Outside, symbol: Symbol::of_name("d3_outside") }
}

fn names(name: &str) -> Option<Target> {
    (name == "d3_outside").then(outside)
}

fn object(ir: &str, machine: &Machine, cpu: &str) -> AVec<'static, u8> {
    init().expect("LLVM's targets register");
    let alloc = alloc();
    let mut out: AVec<'static, u8> = Vec::new_in(alloc);
    let mut remarks: AVec<'static, Remark> = Vec::new_in(alloc);
    compile(ir.as_bytes(), machine, cpu, Pipeline::BlazeO2, &mut out, &mut remarks, alloc).expect("the IR compiles");
    out
}

fn record(ir: &str, machine: &Machine, cpu: &str, symbol: &str) -> Result<Emitted<'static>, ReadError> {
    let obj = object(ir, machine, cpu);
    read_object(&obj, symbol, &names, alloc())
}

/// The kinds a record of `F_IR` must hold: the call to the outside name, and the table's page
/// and page offset in the record's own relro.
fn assert_f_shape(e: &Emitted<'_>) {
    let call = e.relocs.iter().find(|r| r.kind == RelocKind::Branch26).expect("the call is a Branch26");
    assert_eq!(e.targets[call.target as usize], outside(), "the call reaches d3_outside");
    assert!(e.relocs.iter().any(|r| r.kind == RelocKind::Page21 && r.target == OWN_RELRO), "the table's page is the record's relro");
    assert!(e.relocs.iter().any(|r| matches!(r.kind, RelocKind::PageOff12 { .. }) && r.target == OWN_RELRO), "the table's offset is the record's relro");
    assert_eq!(e.unwind_frame, Some(Frame::PLAIN), "the frame record only: nothing is live across the call");
    let table: std::vec::Vec<u8> = TABLE.iter().flat_map(|v| v.to_le_bytes()).collect();
    assert!(e.relro.windows(table.len()).any(|w| w == table.as_slice()), "the table's bytes are in the relro part");
}

/// The run's answers: `d3_outside`'s entry thunk; no import word is asked for (LLVM calls an
/// outside name with `bl`).
struct Run {
    thunk: u64,
}

impl Words for Run {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        match target == outside() {
            true => Ok(self.thunk),
            false => Err(ImageError::Unbound { target }),
        }
    }
}

/// The sealed record's bytes, read back as the placer reads them.
fn sealed_bytes(e: Emitted<'static>) -> AVec<'static, u8> {
    let sealed = seal(e).expect("the record seals");
    let mut bytes: AVec<'static, u8> = Vec::new_in(alloc());
    encode(&sealed, &mut bytes);
    bytes
}

/// D1 compiles a function calling an outside one; D3 reads the Mach-O object back; R's `place`
/// puts it beside an entry thunk for `d3_outside`; it runs to the oracle's values.
/// Passes: "test a_call_to_an_outside_name_reads_places_and_runs ... ok".
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn a_call_to_an_outside_name_reads_places_and_runs() {
    let e = record(F_IR, &MAC, MAC_CPU, "d3_f").expect("the object reads");
    assert_f_shape(&e);
    let bytes = sealed_bytes(e);
    let view = read(&bytes).expect("the sealed record reads back");
    let code_len = view.code().len();
    let relro_len = view.relro().len();
    let page = jit::PAGE;
    assert!(64 + code_len <= page && relro_len <= page, "the test's two pages hold the record");
    let base = jit::map(2 * page);
    let thunk = base as u64;
    let code_at = thunk + 64;
    let relro_at = thunk + page as u64;
    jit::writable(true);
    // SAFETY: `base` is a fresh mapping of two pages, writable on this thread now; the thunk
    // (16 bytes), the code (at 64, under a page) and the relro part (the second page) do not
    // overlap and stay inside it.
    unsafe {
        let words = [0x5800_0050u32, 0xD61F_0200u32];
        core::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), base, 8);
        let target = d3_outside as extern "C" fn(i64) -> i64 as usize as u64;
        core::ptr::copy_nonoverlapping(target.to_le_bytes().as_ptr(), base.add(8), 8);
        let code = core::slice::from_raw_parts_mut(base.add(64), code_len);
        let relro = core::slice::from_raw_parts_mut(base.add(page), relro_len);
        let mut run = Run { thunk };
        place(&view, Symbol::of_name("d3_f"), At { code: code_at, relro: relro_at }, &mut run, code, relro).expect("the record places");
    }
    jit::writable(false);
    jit::flush(base, page);
    // SAFETY: `code_at` holds `d3_f` as placed, executable on this thread now, with the C
    // calling convention LLVM compiled it for (two i64 in x0 and x1, one out in x0).
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { core::mem::transmute::<usize, extern "C" fn(i64, i64) -> i64>(code_at as usize) };
    for (a, b) in [(0, 0), (1, 2), (2, -5), (3, 1 << 40), (-1, 7), (i64::MAX, 1), (i64::MIN, -1), (12345, 678)] {
        assert_eq!(f(a, b), expected(a, b), "d3_f({a}, {b})");
    }
    jit::unmap(base, 2 * page);
}

/// The same function as an ELF object for aarch64 Linux reads to the same shape; placed at an
/// address, its `bl` reaches the thunk (not run: the host is a Mac).
/// Passes: "test the_elf_object_reads_to_the_same_shape ... ok".
#[test]
fn the_elf_object_reads_to_the_same_shape() {
    let e = record(F_IR, &LINUX, LINUX_CPU, "d3_f").expect("the object reads");
    assert_f_shape(&e);
    let call_at = e.relocs.iter().find(|r| r.kind == RelocKind::Branch26).map(|r| u64::from(r.at)).expect("a call");
    let bytes = sealed_bytes(e);
    let view = read(&bytes).expect("the sealed record reads back");
    let at = At { code: 0x1000_0000, relro: 0x1001_0000 };
    let thunk = 0x0FFF_FF00u64;
    let mut code = std::vec![0u8; view.code().len()];
    let mut relro = std::vec![0u8; view.relro().len()];
    place(&view, Symbol::of_name("d3_f"), at, &mut Run { thunk }, &mut code, &mut relro).expect("the record places");
    let inst = u32::from_le_bytes(code[call_at as usize..call_at as usize + 4].try_into().expect("four bytes"));
    assert_eq!(inst & 0xFC00_0000, 0x9400_0000, "a bl at the call's relocation");
    let delta = (((inst & 0x03FF_FFFF) << 6) as i32 >> 6) as i64 * 4;
    assert_eq!((at.code + call_at).wrapping_add_signed(delta), thunk, "the bl reaches d3_outside's thunk");
}

/// On Mach-O, LLVM saves `%a`'s callee-saved register in the platform's pair order (x19 at
/// CFA - 24, x20 at CFA - 32), which is blaze's one frame form (unwind-P8a-4): it reads to a frame
/// with the pair x19:x20. On ELF, LLVM puts the frame record below the saved registers, which the
/// form cannot say: refused by name, never converted.
/// Passes: "test llvms_saved_pair_is_the_one_frame_form ... ok".
#[test]
fn llvms_saved_pair_is_the_one_frame_form() {
    let e = record(G_IR, &MAC, MAC_CPU, "d3_g").unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(e.unwind_frame, Some(Frame { x_pairs: 1, d_pairs: 0 }), "the frame record and the pair x19:x20");
    let error = record(G_IR, &LINUX, LINUX_CPU, "d3_g").err().expect("the ELF frame is refused");
    assert!(matches!(error, ReadError::FrameLayout { .. }), "{error}");
}

/// D100 point 10 ("A code record carries its unwind rows and landing pads"): a frame LLVM
/// describes in Mach-O's `__TEXT,__eh_frame` (the compact entry in DWARF mode) is READ, by the
/// same call-frame reader as ELF's `.eh_frame`, never refused as a section. `d3_h` saves `x30`
/// with no frame record, so what is read is refused by the frame form's rule (`FrameLayout`:
/// the CFA is `sp + 16`, not `x29 + 16`), not by the section's name. The first assertion is the
/// test's premise: LLVM put the frame in `__eh_frame`.
/// Passes: "test a_mach_o_eh_frame_is_read_not_refused_as_a_section ... ok".
#[test]
fn a_mach_o_eh_frame_is_read_not_refused_as_a_section() {
    let obj = object(H_IR, &MAC, MAC_CPU);
    assert!(obj.windows(10).any(|w| w == b"__eh_frame"), "LLVM described d3_h's frame in __eh_frame (the test's premise)");
    let error = read_object(&obj, "d3_h", &names, alloc()).err().expect("a frame with no frame record is refused");
    assert!(matches!(error, ReadError::FrameLayout { .. }), "{error}");
}

/// A thread-local's relocation has no row in the record: refused by its name. The Mach-O object
/// also carries `__TEXT,__eh_frame`; the code's relocations are read before any unwind section,
/// so the refusal names the relocation.
/// Passes: "test a_relocation_with_no_row_is_refused_by_name ... ok".
#[test]
fn a_relocation_with_no_row_is_refused_by_name() {
    let error = record(T_IR, &MAC, MAC_CPU, "d3_t").err().expect("the TLV load is refused");
    match error {
        ReadError::Relocation { kind, .. } => assert!(kind.starts_with("ARM64_RELOC_TLVP_LOAD"), "{error}"),
        other => panic!("not a relocation refusal: {other}"),
    }
    let error = record(T_IR, &LINUX, LINUX_CPU, "d3_t").err().expect("the TLS access is refused");
    assert!(matches!(error, ReadError::Relocation { .. } | ReadError::SymbolKind { .. }), "{error}");
}

/// x86-64: the record has `Pc32` and `GotPc32` rows, and this reader writes none of them yet,
/// so an x86-64 object's first relocation is refused by its `R_X86_64_*` name, never read as an
/// arm64 row.
/// Passes: "test an_x86_64_relocation_is_refused_by_name ... ok".
#[test]
fn an_x86_64_relocation_is_refused_by_name() {
    let obj = object(F_IR, &X86_LINUX, X86_CPU);
    let error = read_object(&obj, "d3_f", &names, alloc()).err().expect("the x86-64 object is refused");
    match error {
        ReadError::NotYetSited { kind, .. } => assert!(kind.starts_with("R_X86_64_"), "{error}"),
        other => panic!("not refused by the relocation's name: {other}"),
    }
}

/// Bytes that are no object, or either object cut at EVERY length short of whole, are refused,
/// never read past: every field read is bounds-checked, and the table LLVM writes last (Mach-O's
/// string table, ELF's section header table) is checked whole, so every cut falls in a checked
/// range. And every one-byte change of either object reads or is refused, never panics: a
/// record is shared, so the reader takes its bytes as untrusted.
/// Passes: "test cut_and_foreign_bytes_are_refused ... ok".
#[test]
fn cut_and_foreign_bytes_are_refused() {
    let alloc = alloc();
    let error = read_object(b"not an object", "f", &names, alloc).err().expect("refused");
    assert!(matches!(error, ReadError::Format { .. }), "{error}");
    for (format, machine, cpu) in [("Mach-O", &MAC, MAC_CPU), ("ELF", &LINUX, LINUX_CPU)] {
        let obj = object(F_IR, machine, cpu);
        assert!(read_object(&obj, "d3_f", &names, alloc).is_ok(), "the whole {format} object reads");
        for cut in 0..obj.len() {
            let error = read_object(&obj[..cut], "d3_f", &names, alloc).err();
            assert!(error.is_some(), "a {format} object cut at {cut} of {} bytes is refused", obj.len());
        }
        let mut changed: std::vec::Vec<u8> = obj.iter().copied().collect();
        let mut refused = 0usize;
        for at in 0..changed.len() {
            let was = changed[at];
            changed[at] = !was;
            refused += usize::from(read_object(&changed, "d3_f", &names, alloc).is_err());
            changed[at] = was;
        }
        assert!(refused > 0, "some one-byte change of the {format} object is refused (its magic, at least)");
    }
}

/// The executable mapping of the run test (macOS on arm64: `MAP_JIT`, toggled per thread).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod jit {
    use core::ffi::c_void;

    pub const PAGE: usize = 0x4000;
    const PROT_RWX: i32 = 1 | 2 | 4;
    const MAP_PRIVATE: i32 = 0x2;
    const MAP_ANON: i32 = 0x1000;
    const MAP_JIT: i32 = 0x800;

    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
        fn pthread_jit_write_protect_np(enabled: i32);
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    pub fn map(len: usize) -> *mut u8 {
        // SAFETY: an anonymous private mapping at an address the kernel picks; nothing aliases it.
        let at = unsafe { mmap(core::ptr::null_mut(), len, PROT_RWX, MAP_PRIVATE | MAP_ANON | MAP_JIT, -1, 0) };
        assert!(at as isize != -1, "mmap of {len} bytes");
        at.cast::<u8>()
    }

    /// `MAP_JIT` pages are writable or executable per thread: `true` for writing.
    pub fn writable(on: bool) {
        // SAFETY: switches this thread's view of MAP_JIT pages only.
        unsafe { pthread_jit_write_protect_np(i32::from(!on)) }
    }

    pub fn flush(at: *mut u8, len: usize) {
        // SAFETY: `at..at + len` is inside the mapping just written.
        unsafe { sys_icache_invalidate(at.cast::<c_void>(), len) }
    }

    pub fn unmap(at: *mut u8, len: usize) {
        // SAFETY: `at` is the mapping `map` made of `len` bytes; nothing points into it after.
        let done = unsafe { munmap(at.cast::<c_void>(), len) };
        assert_eq!(done, 0, "munmap");
    }
}
