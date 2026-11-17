//! Stack probes (pass/probe.rs; THREADS.md D3). The rule, from the pinned library
//! (std/src/sys/pal/unix/stack_overflow/handler_signal.rs, 19-20 and 109): "We've got guard
//! pages (unmapped pages) at the end of every thread's stack, so if a thread ends up running
//! into the guard page it'll trigger" ...; "Even for panic=immediate-abort, installing the
//! guard pages is important for soundness." So no frame may move `sp` past a guard without
//! writing inside it: every untouched stretch on the way down is under 4096 bytes, the
//! smallest page among the hosts.
//!
//! Three tests: the tail's words field by field (`the_probe_words`); the prologue and the
//! tail decoded and walked for frames each side of the bound
//! (`a_1_mib_frame_touches_each_4096_bytes`); and the cause itself, natively, in a child
//! process whose thread runs on a stack with a guard and a canary under it
//! (`the_guard_stops_a_320_kib_frame`).

use std::alloc::Global;

use blaze_lir::{
    Builder, Full,
    builder::Variadic,
    func::{BlockKind, Conv, Func, SlotKind},
};

use crate::{
    enc::{self, Cond, SP, Size, ZR},
    enc_probe::{self, X17},
    tests::emitted,
};

/// The step and bound of pass/probe.rs's `PROBE_STEP` (that constant is private to the pass).
const STEP: u64 = 4096;

/// The words the tail uses that src/enc.rs had no form for, against the Arm ARM's fields, and
/// the enc.rs forms the tail takes, against the words the module doc's text assembles to.
#[test]
fn the_probe_words() {
    // `nop`: HINT #0.
    assert_eq!(enc_probe::NOP, 0xd503_201f, "nop");
    // `sub sp, sp, x17` (extended, UXTX): sf=1 op=1 S=0 01011 00 1 Rm=10001 011 000 11111 11111.
    assert_eq!(enc_probe::sub_ext_uxtx(SP, SP, X17), 0b1_1_0_01011_00_1_10001_011_000_11111_11111, "sub sp, sp, x17, uxtx");
    assert_eq!(enc_probe::sub_ext_uxtx(SP, SP, X17), 0xcb31_63ff, "sub sp, sp, x17, uxtx");
    // `cmp x17, #1, lsl #12`: sf=1 op=1 S=1 100010 sh=1 imm12=1 Rn=10001 Rd=11111.
    assert_eq!(enc_probe::cmp_imm_lsl12(X17, 1), 0b1_1_1_100010_1_000000000001_10001_11111, "cmp x17, #1, lsl #12");
    assert_eq!(enc_probe::cmp_imm_lsl12(X17, 1), 0xf140_063f, "cmp x17, #1, lsl #12");
    // enc.rs's forms, as the tail calls them.
    assert_eq!(enc::sub_imm(SP, SP, 1, true), 0xd140_07ff, "sub sp, sp, #1, lsl #12");
    assert_eq!(enc::sub_imm(X17, X17, 1, true), 0xd140_0631, "sub x17, x17, #1, lsl #12");
    assert_eq!(enc::str(Size::X, ZR, SP, 0), 0xf900_03ff, "str xzr, [sp]");
    assert_eq!(enc::movz(X17, 0x1234, 0), 0xd282_4691, "movz x17, #0x1234");
    assert_eq!(enc::movk(X17, 0x10, 1), 0xf2a0_0211, "movk x17, #0x10, lsl #16");
    assert_eq!(enc::b_cond(Cond::Hs, -4), 0x54ff_ff82, "b.hs .-16");
    println!("the probe tail's words hold their Arm ARM fields");
}

/// `f() = ret`, with one 16-aligned stack slot of `slot` bytes and nothing else: its frame is
/// one 16-byte cell (the `ret`) and the slot, `(16 + slot)` rounded up to 16
/// (pass.rs, `run` and `settle`).
fn framed(slot: u32) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let _slot = b.slot(slot, 4, SlotKind::Temp);
    b.ret(&[]);
    b.finish_checked().expect("a hand-built function builds")
}

fn word(code: &[u8], at: u64) -> u32 {
    let at = at as usize;
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// The prologue's frame words: `stp`, `add fp`, then the two at this offset.
const FRAME_AT: u64 = 8;

/// What running the tail from `b tail` back to `frame_at + 8` did, from `sp = entry`.
struct Walk {
    /// Each write's address, in order: the `stp` at `entry - 16` first.
    touched: Vec<u64>,
    /// `sp` where the tail branches back.
    sp: u64,
    /// Whether a `b.hs` went back (the tail holds a loop that ran).
    looped: bool,
}

/// Runs the words from `FRAME_AT` on (`b tail` there) until the branch back to `FRAME_AT + 8`,
/// on `sp` and `x17` alone; any word the tail of pass/probe.rs does not hold is refused by its
/// value. Bounded: a frame under 16 MiB takes at most 4096 turns of the loop.
fn walk(code: &[u8], entry: u64) -> Walk {
    let mut touched = vec![entry - 16];
    let (mut sp, mut x17, mut hs, mut looped) = (entry - 16, 0u64, false, false);
    let mut pc = FRAME_AT;
    for _ in 0..(8 * 4096 + 64) {
        let w = word(code, pc);
        let mut next = pc + 4;
        if w & 0xfc00_0000 == 0x1400_0000 {
            // `b`: a signed 26-bit word offset.
            let to = pc.wrapping_add_signed(i64::from(((w << 6) as i32) >> 6) * 4);
            if to == FRAME_AT + 8 {
                return Walk { touched, sp, looped };
            }
            next = to;
        } else if w & 0xffe0_001f == 0xd280_0011 {
            x17 = u64::from((w >> 5) & 0xffff);
        } else if w & 0xffe0_001f == 0xf2a0_0011 {
            x17 = (x17 & !0xffff_0000) | (u64::from((w >> 5) & 0xffff) << 16);
        } else if w & 0xffc0_03ff == 0xd140_03ff {
            sp -= u64::from((w >> 10) & 0xfff) << 12;
        } else if w == 0xf900_03ff {
            touched.push(sp);
        } else if w & 0xffc0_03ff == 0xd140_0231 {
            x17 = x17.checked_sub(u64::from((w >> 10) & 0xfff) << 12).expect("the tail's count never goes below zero");
        } else if w & 0xffc0_03ff == 0xf140_023f {
            hs = x17 >= u64::from((w >> 10) & 0xfff) << 12;
        } else if w & 0xff00_001f == 0x5400_0002 {
            // `b.hs`: a signed 19-bit word offset in 5..24.
            if hs {
                next = pc.wrapping_add_signed(i64::from(((w << 8) as i32) >> 13) * 4);
                looped = true;
            }
        } else if w == 0xcb31_63ff {
            sp -= x17;
        } else {
            panic!("a word the probe tail does not hold at {pc}: {w:#010x}");
        }
        pc = next;
    }
    panic!("the probe tail ran past its bound without branching back");
}

/// A frame at the bound keeps its two `sub`s; past it, the prologue's frame word is a `b` to a
/// tail that moves `sp` by exactly the frame and leaves no stretch of 4096 bytes untouched on
/// the way down (the `stp`'s write, each probe, and the frame's lowest byte, from an entry `sp`
/// on a page boundary: the case where a whole untouched 4 KiB is a whole guard). The 8 KiB
/// frame is a multiple of the step: D3's `b.hi` leaves 4096 bytes untouched there and fails.
/// Prints "a 1 MiB frame touches each 4096 bytes".
#[test]
fn a_1_mib_frame_touches_each_4096_bytes() {
    // 4064: frame 4080, 16 + 4080 = 4096, at the bound: no probe.
    let code = emitted(framed(4064).view(), &[]).expect("emits");
    let bytes = &code.bytes;
    assert_eq!(word(bytes, FRAME_AT), enc::sub_imm(SP, SP, 0, true), "a 4080-byte frame keeps its first `sub`");
    assert_eq!(word(bytes, FRAME_AT + 4), enc::sub_imm(SP, SP, 4080, false), "a 4080-byte frame keeps its second `sub`");

    let entry: u64 = 1 << 40;
    for slot in [4096u32, 8176, (1 << 20) - 16, 1 << 20] {
        let frame = (16 + u64::from(slot)).next_multiple_of(16);
        let code = emitted(framed(slot).view(), &[]).expect("emits");
        let bytes = &code.bytes;
        let first = word(bytes, FRAME_AT);
        assert_eq!(first & 0xfc00_0000, 0x1400_0000, "a {frame}-byte frame's prologue word is a `b`: {first:#010x}");
        assert_eq!(word(bytes, FRAME_AT + 4), enc_probe::NOP, "a {frame}-byte frame's second word is a `nop`");
        let walk = walk(bytes, entry);
        assert!(walk.looped || frame < 2 * STEP, "a {frame}-byte frame's tail holds a loop that ran");
        assert_eq!(walk.sp, entry - 16 - frame, "a {frame}-byte frame's tail moves `sp` by the frame");
        for pair in walk.touched.windows(2) {
            let (above, below) = (pair[0], pair[1]);
            assert!(below < above, "a {frame}-byte frame's writes go down");
            let gap = above - (below + 8);
            assert!(gap < STEP, "a {frame}-byte frame leaves {gap} bytes untouched between writes at {above:#x} and {below:#x}");
        }
        let last = *walk.touched.last().expect("the `stp` wrote");
        let rest = last - walk.sp;
        assert!(rest < STEP, "a {frame}-byte frame leaves {rest} bytes untouched under its last write");
        assert!(walk.touched.len() as u64 >= frame / STEP, "a {frame}-byte frame touches each step");
    }
    println!("a 1 MiB frame touches each 4096 bytes");
}

/// The child's switch: the canary file's path.
#[cfg(all(target_arch = "aarch64", unix))]
const PROBE_CHILD: &str = "BLAZE_EMIT_PROBE_CHILD";

/// The cause, natively. A child process (this test binary again) maps, low to high, [canary
/// 64 KiB, shared with a file the parent reads][guard 16 KiB, `PROT_NONE`][stack 256 KiB],
/// starts a thread on the stack (`pthread_attr_setstack`) and calls an emitted function with a
/// 320 KiB frame that stores 1 to its lowest slot. With probes, the walk down faults in the
/// guard and the child dies by SIGSEGV or SIGBUS with the canary all zero; without them, the
/// frame steps over the guard, the store lands in the canary and the child exits 0: the test
/// fails both ways. Prints "the guard stops a 320 KiB frame: child killed by signal, canary
/// untouched".
#[cfg(all(target_arch = "aarch64", unix))]
#[test]
fn the_guard_stops_a_320_kib_frame() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if let Ok(path) = std::env::var(PROBE_CHILD) {
        native::child(&path);
        return;
    }
    let path = std::env::temp_dir().join(format!("blaze-emit-probe-canary-{}", std::process::id()));
    std::fs::write(&path, vec![0u8; native::CANARY]).expect("the canary file is written");
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut child = Command::new(&exe)
        .args(["--exact", "probe_tests::the_guard_stops_a_320_kib_frame", "--test-threads=1"])
        .env(PROBE_CHILD, &path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the child starts");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is waited on") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = std::fs::remove_file(&path);
            panic!("the probe child ran past its 60 s deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let canary = std::fs::read(&path).expect("the canary file reads back");
    std::fs::remove_file(&path).expect("the canary file is removed");
    let written = canary.iter().filter(|&&b| b != 0).count();
    assert_eq!(written, 0, "{written} bytes of the mapping under the guard were written (the frame stepped over it); the child ended {status:?}");
    assert!(
        matches!(status.signal(), Some(native::SIGSEGV | native::SIGBUS)),
        "the child ended {status:?}, not by SIGSEGV or SIGBUS at the guard"
    );
    println!("the guard stops a 320 KiB frame: child killed by signal, canary untouched");
}

#[cfg(all(target_arch = "aarch64", unix))]
mod native {
    use core::ffi::{c_int, c_void};
    use core::ptr;
    use std::alloc::Global;
    use std::os::fd::AsRawFd;

    use blaze_lir::{Flags, Ty, builder::MemAttrs, func::{Func, SlotKind}};

    use crate::tests::{emitted, one, run};

    pub(super) const CANARY: usize = 64 << 10;
    const GUARD: usize = 16 << 10;
    const STACK: usize = 256 << 10;
    /// The frame's slot: 64 KiB past the stack, so with the few hundred bytes to some KiB the
    /// thread's start used above, the unprobed store lands inside the canary.
    const SLOT: u32 = 320 << 10;
    /// The handler's own stack, which the faulting thread needs: its `sp` is in the guard.
    const ALT_STACK: usize = 64 << 10;

    pub(super) const SIGSEGV: i32 = 11;
    #[cfg(target_os = "macos")]
    pub(super) const SIGBUS: i32 = 10;
    #[cfg(target_os = "linux")]
    pub(super) const SIGBUS: i32 = 7;

    const PROT_NONE: c_int = 0;
    const PROT_RW: c_int = 1 | 2;
    const MAP_SHARED: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    const MAP_FIXED: c_int = 0x10;
    #[cfg(target_os = "macos")]
    const MAP_ANON: c_int = 0x1000;
    #[cfg(target_os = "linux")]
    const MAP_ANON: c_int = 0x20;

    /// `pthread_attr_t`: 64 bytes on both hosts (macOS: `long __sig; char __opaque[56]`;
    /// glibc on aarch64: `__SIZEOF_PTHREAD_ATTR_T` 64), set up by `pthread_attr_init` only.
    #[repr(C, align(16))]
    struct Attr([u8; 64]);

    /// `stack_t`, whose field order differs between the hosts.
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct AltStack {
        sp: *mut c_void,
        size: usize,
        flags: c_int,
    }
    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct AltStack {
        sp: *mut c_void,
        flags: c_int,
        size: usize,
    }

    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn sigaltstack(new: *const AltStack, old: *mut AltStack) -> c_int;
        fn pthread_attr_init(attr: *mut Attr) -> c_int;
        fn pthread_attr_setstack(attr: *mut Attr, addr: *mut c_void, size: usize) -> c_int;
        fn pthread_create(
            thread: *mut usize,
            attr: *const Attr,
            start: extern "C" fn(*mut c_void) -> *mut c_void,
            arg: *mut c_void,
        ) -> c_int;
        fn pthread_join(thread: usize, out: *mut *mut c_void) -> c_int;
    }

    fn failed(p: *mut c_void) -> bool {
        p as usize == usize::MAX
    }

    /// `f() = { *slot(320 KiB) = 1; 0 }`: the store at the slot's first byte, the frame's
    /// lowest stretch above the cells.
    fn storing() -> Func<Global> {
        one(&[], Ty::I64, |b, _| {
            let slot = b.slot(SLOT, 4, SlotKind::Temp);
            let at = b.slot_addr(slot);
            let value = b.iconst(Ty::I64, 1);
            b.store(Flags::NONE, value, at, 0, MemAttrs::default());
            b.iconst(Ty::I64, 0)
        })
    }

    /// The thread's start: an alternate signal stack for this thread (the fault's handler
    /// cannot run on a stack whose `sp` is in the guard), then the emitted function at `arg`.
    extern "C" fn enter(arg: *mut c_void) -> *mut c_void {
        // SAFETY: an anonymous private mapping of a fresh range: no address is named, no
        // file, nothing else aliases it.
        let alt = unsafe { mmap(ptr::null_mut(), ALT_STACK, PROT_RW, MAP_PRIVATE | MAP_ANON, -1, 0) };
        assert!(!failed(alt), "the alternate signal stack maps");
        let stack = AltStack { sp: alt, size: ALT_STACK, flags: 0 };
        // SAFETY: `stack` names the mapping just made, readable and writable, and outlives the
        // thread (it is never unmapped: the process ends right after).
        assert_eq!(unsafe { sigaltstack(&stack, ptr::null_mut()) }, 0, "the alternate signal stack is set");
        // SAFETY: `arg` is the first byte of an emitted function of the C convention taking
        // nothing and returning an `i64`, mapped executable by the caller's `Executable`,
        // which outlives this thread (the caller joins it first).
        let f = unsafe { core::mem::transmute::<*mut c_void, extern "C" fn() -> u64>(arg) };
        f();
        ptr::null_mut()
    }

    /// The child's work (see the test's doc). Returns only if the function returned: no fault.
    pub(super) fn child(path: &str) {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).expect("the canary file opens");
        let total = CANARY + GUARD + STACK;
        // SAFETY: an anonymous private mapping of a fresh range, as in `enter`.
        let base = unsafe { mmap(ptr::null_mut(), total, PROT_RW, MAP_PRIVATE | MAP_ANON, -1, 0) };
        assert!(!failed(base), "the region maps");
        // SAFETY: `MAP_FIXED` replaces the first 64 KiB of the region just mapped, which
        // nothing else uses; the file is 64 KiB long (the parent wrote it) and open for
        // reading and writing.
        let canary = unsafe { mmap(base, CANARY, PROT_RW, MAP_SHARED | MAP_FIXED, file.as_raw_fd(), 0) };
        assert_eq!(canary, base, "the canary maps over the region's low end");
        // SAFETY: `base + CANARY` is inside the region (and page-aligned: 64 KiB is a whole
        // number of pages on both hosts); the guard's 16 KiB end inside it too.
        let guard = unsafe { base.byte_add(CANARY) };
        assert_eq!(unsafe { mprotect(guard, GUARD, PROT_NONE) }, 0, "the guard is made inaccessible");
        let code = emitted(storing().view(), &[]).expect("emits");
        let exec = run::place(&code, |_| unreachable!());
        let mut attr = Attr([0; 64]);
        // SAFETY: `attr` is 64 bytes, aligned, as both hosts' `pthread_attr_t`.
        assert_eq!(unsafe { pthread_attr_init(&mut attr) }, 0, "the thread's attributes start");
        // SAFETY: the stack is the region's top 256 KiB, page-aligned and a whole number of
        // pages, readable and writable, above the guard, and outlives the thread.
        let stack = unsafe { base.byte_add(CANARY + GUARD) };
        assert_eq!(unsafe { pthread_attr_setstack(&mut attr, stack, STACK) }, 0, "the thread's stack is set");
        let mut thread = 0usize;
        // SAFETY: `enter` is the C-convention start; its argument is `exec`'s code, which lives
        // until after the join below; `thread` is pointer-sized on both hosts (`pthread_t`).
        let made = unsafe { pthread_create(&mut thread, &attr, enter, exec.code_address() as *mut c_void) };
        assert_eq!(made, 0, "the thread starts");
        // SAFETY: `thread` was started above and is joined once.
        assert_eq!(unsafe { pthread_join(thread, ptr::null_mut()) }, 0, "the thread is joined");
        drop(exec);
        drop(file);
    }
}
