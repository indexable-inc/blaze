//! [`Executable`] runs what it was given: checked against the mapping blaze-lazy's and
//! blaze-stencil's tests each made for themselves before it (`reference`), and for the data's
//! reach from the code by a PC-relative load.

#![cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
))]

use super::{
    Executable, JitPool, LinkUnit, Reloc, RelocKind, RelocTarget, apply_reloc,
};

/// `mov eax, 42; ret` / `mov w0, #42; ret`.
#[cfg(target_arch = "x86_64")]
const FORTY_TWO: &[u8] = &[0xb8, 42, 0, 0, 0, 0xc3];
#[cfg(target_arch = "aarch64")]
const FORTY_TWO: &[u8] = &[0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];

/// Calls the C function `() -> u32` at `at`.
fn call(at: u64) -> u32 {
    // SAFETY: every caller passes the address of a function of this signature in the C
    // convention, mapped executable and live for the call.
    let ptr = at as usize as *const ();
    const {
        assert!(size_of::<*const ()>() == size_of::<extern "C" fn() -> u32>());
        assert!(align_of::<*const ()>() == align_of::<extern "C" fn() -> u32>());
    }
    let f: extern "C" fn() -> u32 = unsafe { core::ptr::read((&raw const ptr).cast()) };
    f()
}

/// The code bytes at `at`, `len` of them.
fn code_at(at: u64, len: usize) -> Vec<u8> {
    // SAFETY: `at..at + len` is code of a live `Executable` or reference mapping, readable and
    // never written again.
    unsafe { std::slice::from_raw_parts(at as *const u8, len) }.to_vec()
}

/// The test-only mapping each test crate kept: anonymous memory written, then made
/// executable, with the cache maintenance by hand.
#[cfg(unix)]
mod reference {
    use blaze_sys::posix::io::{self, MapFlags, Prot};

    pub(super) struct Code {
        pub(super) at: *mut u8,
        len: usize,
    }

    impl Drop for Code {
        fn drop(&mut self) {
            // SAFETY: the mapping `new` made; no code of it runs once the test drops it.
            let _ = unsafe { io::munmap(std::ptr::NonNull::new_unchecked(self.at), self.len) };
        }
    }

    impl Code {
        pub(super) fn new(bytes: &[u8]) -> Code {
            let len = bytes.len().next_multiple_of(1 << 16);
            let (protection, flags) =
                (Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS);
            // SAFETY: an anonymous mapping at no fixed address; the result is checked.
            let at = unsafe { io::mmap(None, len, protection, flags, None, 0) }
                .unwrap_or_else(|errno| panic!("mmap: {}", std::io::Error::from_raw_os_error(errno.raw().into())));
            let code = Code { at: at.as_ptr(), len };
            // SAFETY: the fresh mapping of `len >= bytes.len()` writable bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), code.at, bytes.len()) };
            // SAFETY: the whole mapping made above, which nothing writes from here on.
            unsafe { io::mprotect(at, len, Prot::READ | Prot::EXEC) }
                .unwrap_or_else(|errno| panic!("mprotect: {}", std::io::Error::from_raw_os_error(errno.raw().into())));
            #[cfg(target_os = "macos")]
            // SAFETY: the instruction cache sees the bytes in the fresh mapping.
            unsafe {
                sys_icache_invalidate(code.at.cast(), bytes.len())
            };
            #[cfg(all(not(target_os = "macos"), target_arch = "aarch64"))]
            icache(code.at as usize, bytes.len());
            code
        }
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut std::ffi::c_void, len: usize);
    }

    #[cfg(all(not(target_os = "macos"), target_arch = "aarch64"))]
    fn icache(start: usize, len: usize) {
        use std::arch::asm;

        let ctr: u64;
        // SAFETY: Linux lets EL0 read CTR_EL0.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
        let (data, instruction) = (4usize << ((ctr >> 16) & 0xf), 4usize << (ctr & 0xf));
        let end = start + len;
        let mut line = start & !(data - 1);
        while line < end {
            // SAFETY: a cache clean of a mapped address.
            unsafe { asm!("dc cvau, {}", in(reg) line, options(nostack, preserves_flags)) };
            line += data;
        }
        // SAFETY: a barrier.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
        let mut line = start & !(instruction - 1);
        while line < end {
            // SAFETY: an instruction cache invalidate of a mapped address.
            unsafe { asm!("ic ivau, {}", in(reg) line, options(nostack, preserves_flags)) };
            line += instruction;
        }
        // SAFETY: barriers.
        unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
    }
}

#[cfg(unix)]
#[test]
fn runs_what_the_reference_mapping_runs() {
    let reference = reference::Code::new(FORTY_TWO);
    let exec = Executable::new(FORTY_TWO, 0).unwrap();
    assert_eq!(
        code_at(exec.code_address(), FORTY_TWO.len()),
        code_at(reference.at as u64, FORTY_TWO.len())
    );
    assert_eq!(call(exec.code_address()), call(reference.at as u64));
    assert_eq!(call(exec.code_address()), 42);
}

#[test]
fn runs_its_code() {
    let exec = Executable::new(FORTY_TWO, 0).unwrap();
    assert_eq!(code_at(exec.code_address(), FORTY_TWO.len()), FORTY_TWO);
    assert_eq!(call(exec.code_address()), 42);
}

/// A function returning the `u32` at the data's first byte, reached PC-relatively.
fn load_first_word(code: &mut [u8], data: &[u8]) {
    let (code_at, data_at) = (code.as_ptr() as i64, data.as_ptr() as i64);
    #[cfg(target_arch = "x86_64")]
    {
        // mov eax, [rip + disp32]; ret
        let disp = i32::try_from(data_at - (code_at + 6)).expect("the data in reach");
        code[..2].copy_from_slice(&[0x8b, 0x05]);
        code[2..6].copy_from_slice(&disp.to_le_bytes());
        code[6] = 0xc3;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // adrp x1, data; ldr w0, [x1, #lo12]; ret
        let pages = (data_at >> 12) - (code_at >> 12);
        assert!((-(1 << 20)..1 << 20).contains(&pages), "the data in reach");
        let pages = pages as u32;
        let adrp = 0x9000_0001 | ((pages & 3) << 29) | (((pages >> 2) & 0x7_ffff) << 5);
        let lo12 = (data_at & 0xfff) as u32;
        let ldr = 0xb940_0020 | ((lo12 / 4) << 10);
        for (at, word) in [adrp, ldr, 0xd65f_03c0].into_iter().enumerate() {
            code[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
}

#[test]
fn its_code_reaches_its_data() {
    let mut exec =
        Executable::written(16, 3 << 16, |code, data| load_first_word(code, data)).unwrap();
    assert!(exec.data_address().abs_diff(exec.code_address()) < 1 << 30);
    assert_eq!(call(exec.code_address()), 0, "the data starts zeroed");
    exec.data()[..4].copy_from_slice(&0x5eed_f00du32.to_le_bytes());
    assert_eq!(call(exec.code_address()), 0x5eed_f00d);
}

#[test]
fn its_data_is_zeroed_and_writable() {
    let len = (1 << 16) + 5;
    let mut exec = Executable::new(FORTY_TWO, len).unwrap();
    let data = exec.data();
    assert_eq!(data.len(), len);
    assert!(data.iter().all(|it| *it == 0));
    data.fill(0xa5);
    assert!(exec.data().iter().all(|it| *it == 0xa5));
    assert_eq!(exec.data_address(), exec.data().as_ptr() as u64);
    assert_eq!(call(exec.code_address()), 42);
}

/// Code of an `Executable` runs while another is written, on the writing thread: no
/// per-thread switch makes one's pages unrunnable while another's are writable (what macOS
/// `MAP_JIT` did, so a program's own JIT could stop blaze's code).
#[test]
fn code_runs_while_another_executable_is_written() {
    let first = Executable::new(FORTY_TWO, 0).unwrap();
    let mut ran = 0;
    let second = Executable::written(FORTY_TWO.len(), 0, |code, _| {
        ran = call(first.code_address());
        code.copy_from_slice(FORTY_TWO);
    })
    .unwrap();
    assert_eq!((ran, call(second.code_address())), (42, 42));
}

/// A `write` that panics leaves the other `Executable`s running.
#[test]
fn a_write_that_panics_leaves_the_others_running() {
    let first = Executable::new(FORTY_TWO, 0).unwrap();
    let panicked = std::panic::catch_unwind(|| {
        Executable::written(16, 16, |_, _| panic!("a write that fails"))
    });
    assert!(panicked.is_err());
    assert_eq!(call(first.code_address()), 42);
    assert_eq!(call(Executable::new(FORTY_TWO, 0).unwrap().code_address()), 42);
}

/// A child forked right after its parent wrote an `Executable` runs the code with nothing to
/// do first. The write blocks, so the kernel switches the thread out mid-write: with macOS
/// `MAP_JIT` the child's thread started with the state saved then, and its first instruction
/// fetch from the code faulted (`SIGBUS`) unless it called an `after_fork` first; a private
/// mapping made read and execute keeps that protection across the fork.
#[cfg(unix)]
#[test]
fn a_child_forked_after_a_write_runs_the_code() {
    use blaze_sys::posix::{proc, signal};

    for round in 0..16 {
        let exec = Executable::written(FORTY_TWO.len(), 0, |code, _| {
            code.copy_from_slice(FORTY_TWO);
            // A sleep of this thread, which takes it off its CPU mid-write.
            use blaze_core::wait::{self, Clock as _, Deadline, Monotonic};
            let clock = Monotonic::new();
            let until = Deadline::after(clock.now(), std::time::Duration::from_millis(1));
            wait::sleep_until("a millisecond, mid-write", until, &clock).unwrap();
        })
        .unwrap();
        #[cfg(target_os = "linux")]
        // SAFETY: the child only calls the code (no allocation, no lock) and `_exit`s.
        let pid = unsafe { libc::fork() };
        #[cfg(target_os = "macos")]
        // SAFETY: the child only calls the code (no allocation, no lock) and exits immediately.
        let pid = unsafe { proc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            let got = call(exec.code_address());
            signal::exit_now(got as i32);
        }
        let (waited, status) = proc::wait_pid(pid, proc::WaitFlags::NONE)
            .unwrap_or_else(|errno| panic!("waitpid: {}", std::io::Error::from_raw_os_error(errno.raw().into())))
            .expect("blocking waitpid returned no child");
        assert_eq!(waited, pid);
        assert!(
            status.exit_code() == Some(42),
            "round {round}: the child forked after writing an Executable at {:#x} ended with status {:#x} \
             (signal {}) instead of returning 42 from its code",
            exec.code_address(),
            status.0,
            status.signal().unwrap_or(0),
        );
    }
}

/// Emits a small `extern "C" fn() -> u32` returning `value` (`0..=65535`).
fn emit_ret_u32(value: u16) -> Vec<u8> {
    #[cfg(target_arch = "aarch64")]
    {
        // `movz w0, #value; ret`
        let movz = 0x5280_0000u32 | (u32::from(value) << 5);
        let ret = 0xd65f_03c0u32;
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&movz.to_le_bytes());
        out.extend_from_slice(&ret.to_le_bytes());
        out
    }
    #[cfg(target_arch = "x86_64")]
    {
        // `mov eax, imm32; ret`
        let mut out = Vec::with_capacity(6);
        out.push(0xb8);
        out.extend_from_slice(&u32::from(value).to_le_bytes());
        out.push(0xc3);
        out
    }
}

/// Calls a published `extern "C" fn(u32) -> u32` at `ptr`.
fn call_u32_to_u32(ptr: *const u8, arg: u32) -> u32 {
    // SAFETY: every caller passes an RX pointer to published machine code matching the
    // `extern "C" fn(u32) -> u32` ABI, live in `JitPool` for the duration of the call.
    const {
        assert!(size_of::<*const u8>() == size_of::<extern "C" fn(u32) -> u32>());
        assert!(align_of::<*const u8>() == align_of::<extern "C" fn(u32) -> u32>());
    }
    let f: extern "C" fn(u32) -> u32 = unsafe { core::ptr::read((&raw const ptr).cast()) };
    f(arg)
}

extern "C" fn host_tier_fn_a() -> u32 {
    777
}

extern "C" fn host_tier_fn_b() -> u32 {
    888
}

#[test]
fn jit_pool_emit_publish_call_and_release() {
    assert!(JitPool::new(0).is_err());
    let mut pool = JitPool::new(64 * 1024).unwrap();
    // Dual-mapped W^X invariant: RW and RX are distinct virtual mappings of the same pages.
    assert_ne!(pool.rw_base(), pool.rx_base());
    let initial_free = pool.free_bytes();
    assert_eq!(initial_free, pool.capacity());
    assert!(pool.emit(&[]).is_err());

    // Emit 20 small functions sharing ONE single page (zero per-function mmap/mprotect syscalls).
    let mut handles = Vec::new();
    for k in 1..=20u16 {
        let code = emit_ret_u32(k * 7);
        let h = pool.emit(&code).unwrap();
        assert_eq!(h.len(), code.len());
        assert_eq!(h.allocated_len(), 16);
        handles.push((h, u32::from(k) * 7));
    }
    assert!(
        handles.last().unwrap().0.offset() < pool.page(),
        "all 20 small functions must pack into a single sub-page slab"
    );
    // ONE icache flush for all 20 functions.
    pool.publish();

    for &(h, expected) in &handles {
        let ptr = pool.fn_ptr(h);
        assert_eq!(call(ptr as u64), expected);
        // SAFETY: `h` holds published `extern "C" fn() -> u32` machine code live in `pool`.
        let typed: extern "C" fn() -> u32 = unsafe { pool.typed_fn(h) };
        assert_eq!(typed(), expected);
    }

    // Release even-indexed functions back to `CodeHeap` and verify reuse.
    let before_release = pool.free_bytes();
    let mut released = 0u64;
    for (idx, &(h, _)) in handles.iter().enumerate() {
        if idx % 2 == 0 {
            pool.release(h).unwrap();
            released += h.allocated_len();
        }
    }
    assert_eq!(pool.free_bytes(), before_release + released);

    let mut replacement = Vec::new();
    for k in 100..110u16 {
        let h = pool.emit(&emit_ret_u32(k)).unwrap();
        replacement.push((h, u32::from(k)));
    }
    pool.publish();
    for &(h, expected) in &replacement {
        assert_eq!(call(pool.fn_ptr(h) as u64), expected);
    }
    // Original odd-indexed functions remain untouched and executable.
    for (idx, &(h, expected)) in handles.iter().enumerate() {
        if idx % 2 == 1 {
            assert_eq!(call(pool.fn_ptr(h) as u64), expected);
            pool.release(h).unwrap();
        }
    }
    for (h, _) in replacement {
        pool.release(h).unwrap();
    }
    assert_eq!(pool.free_bytes(), initial_free);
    assert_eq!(pool.largest_free(), initial_free);
}

#[test]
fn jit_pool_multi_function_link_batch_executes() {
    let mut pool = JitPool::new(64 * 1024).unwrap();
    let initial_free = pool.free_bytes();

    #[cfg(target_arch = "aarch64")]
    let (entry_code, entry_relocs, middle_code, middle_relocs, leaf_code, data_bytes, data_relocs) = {
        fn words(ws: &[u32]) -> Vec<u8> {
            let mut out = Vec::with_capacity(ws.len() * 4);
            for w in ws {
                out.extend_from_slice(&w.to_le_bytes());
            }
            out
        }
        // Unit 2 (`leaf(w0) -> w0`): returns `w0 * 3 + 7`.
        //   add w1, w0, w0, lsl #1   (0x0b00_0401)
        //   add w0, w1, #7           (0x1100_1c20)
        //   ret                      (0xd65f_03c0)
        let leaf = words(&[0x0b00_0401, 0x1100_1c20, 0xd65f_03c0]);

        // Unit 1 (`middle(w0) -> w0`): returns `leaf(w0 + 10) + 5`.
        //   stp x29, x30, [sp, #-16]!  (0xa9bf_7bfd)
        //   add w0, w0, #10            (0x1100_2800)
        //   bl  <leaf>                 (0x9400_0000)  @ offset 8
        //   add w0, w0, #5             (0x1100_1400)
        //   ldp x29, x30, [sp], #16    (0xa8c1_7bfd)
        //   ret                        (0xd65f_03c0)
        let middle = words(&[
            0xa9bf_7bfd,
            0x1100_2800,
            0x9400_0000,
            0x1100_1400,
            0xa8c1_7bfd,
            0xd65f_03c0,
        ]);
        let middle_relocs = vec![Reloc::new(8, RelocKind::AArch64Branch26, RelocTarget::Unit(2))];

        // Unit 0 (`entry(w0) -> w0`): returns `middle(w0) + *(data as *const u32)`.
        //   stp x29, x30, [sp, #-16]!  (0xa9bf_7bfd)
        //   bl  <middle>               (0x9400_0000)  @ offset 4
        //   adrp x1, <data>            (0x9000_0001)  @ offset 8
        //   add  x1, x1, #:lo12:<data> (0x9100_0021)  @ offset 12
        //   ldr  w1, [x1]              (0xb940_0021)
        //   add  w0, w0, w1            (0x0b01_0000)
        //   ldp x29, x30, [sp], #16    (0xa8c1_7bfd)
        //   ret                        (0xd65f_03c0)
        let entry = words(&[
            0xa9bf_7bfd,
            0x9400_0000,
            0x9000_0001,
            0x9100_0021,
            0xb940_0021,
            0x0b01_0000,
            0xa8c1_7bfd,
            0xd65f_03c0,
        ]);
        let entry_relocs = vec![
            Reloc::new(4, RelocKind::AArch64Branch26, RelocTarget::Unit(1)),
            Reloc::new(8, RelocKind::AArch64AdrpAdd, RelocTarget::Unit(3)),
        ];

        // Unit 3 (`data`): `[100u32, 0u32, leaf_rx_ptr: u64]`.
        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&100u32.to_le_bytes());
        let data_relocs = vec![Reloc::new(8, RelocKind::Abs64, RelocTarget::Unit(2))];
        (entry, entry_relocs, middle, middle_relocs, leaf, data, data_relocs)
    };

    #[cfg(target_arch = "x86_64")]
    let (entry_code, entry_relocs, middle_code, middle_relocs, leaf_code, data_bytes, data_relocs) = {
        // System V / Windows x86-64: for portability across both ABIs we pass argument in
        // `edi` on Unix or `ecx` on Windows; use `mov eax, arg` accordingly.
        #[cfg(unix)]
        let load_arg: &[u8] = &[0x89, 0xf8]; // mov eax, edi
        #[cfg(windows)]
        let load_arg: &[u8] = &[0x89, 0xc8]; // mov eax, ecx
        #[cfg(unix)]
        let add_arg_10: &[u8] = &[0x83, 0xc7, 10]; // add edi, 10
        #[cfg(windows)]
        let add_arg_10: &[u8] = &[0x83, 0xc1, 10]; // add ecx, 10

        // leaf(x): eax = x * 3 + 7; ret
        let mut leaf = Vec::new();
        leaf.extend_from_slice(load_arg);
        leaf.extend_from_slice(&[0x8d, 0x44, 0x40, 0x07, 0xc3]); // lea eax, [rax + rax*2 + 7]; ret

        // middle(x): add arg, 10; call leaf; add eax, 5; ret
        let mut middle = Vec::new();
        middle.extend_from_slice(add_arg_10);
        let call_off = middle.len() + 1;
        middle.extend_from_slice(&[0xe8, 0, 0, 0, 0]); // call rel32
        middle.extend_from_slice(&[0x83, 0xc0, 5, 0xc3]); // add eax, 5; ret
        let middle_relocs = vec![Reloc::new(call_off, RelocKind::X86Rel32, RelocTarget::Unit(2))];

        // entry(x): call middle; lea rdx, [rip + data]; add eax, [rdx]; ret
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0xe8, 0, 0, 0, 0]); // call middle @ offset 1
        let lea_off = entry.len() + 3;
        entry.extend_from_slice(&[0x48, 0x8d, 0x15, 0, 0, 0, 0]); // lea rdx, [rip + rel32]
        entry.extend_from_slice(&[0x03, 0x02, 0xc3]); // add eax, [rdx]; ret
        let entry_relocs = vec![
            Reloc::new(1, RelocKind::X86Rel32, RelocTarget::Unit(1)),
            Reloc::new(lea_off, RelocKind::X86Rel32, RelocTarget::Unit(3)),
        ];

        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&100u32.to_le_bytes());
        let data_relocs = vec![Reloc::new(8, RelocKind::Abs64, RelocTarget::Unit(2))];
        (entry, entry_relocs, middle, middle_relocs, leaf, data, data_relocs)
    };

    let units = [
        LinkUnit::new(&entry_code, &entry_relocs),
        LinkUnit::new(&middle_code, &middle_relocs),
        LinkUnit::new(&leaf_code, &[]),
        LinkUnit::new(&data_bytes, &data_relocs),
    ];
    let batch = pool.link_batch(&units).unwrap();
    assert_eq!(batch.len(), 4);

    // Call the multi-function linked graph directly on the host CPU!
    // Formula: ((x + 10) * 3 + 7) + 5 + 100 = 3x + 142.
    let entry_ptr = pool.fn_ptr(batch[0]);
    assert_eq!(call_u32_to_u32(entry_ptr, 0), 142);
    assert_eq!(call_u32_to_u32(entry_ptr, 4), 154);
    assert_eq!(call_u32_to_u32(entry_ptr, 10), 172);

    // Verify Abs64 table entry points directly to `leaf` and is callable.
    let data_rw = pool.rw_bytes(batch[3]).unwrap();
    let leaf_addr = u64::from_le_bytes(data_rw[8..16].try_into().unwrap());
    assert_eq!(leaf_addr, pool.rx_address(batch[2]));
    assert_eq!(call_u32_to_u32(leaf_addr as *const u8, 9), 34);

    // Cross-batch linking via `RelocTarget::Handle`: a new unit calls `leaf` from `batch`!
    #[cfg(target_arch = "aarch64")]
    let wrapper = {
        // `b <leaf>` (tail-call `leaf` directly!)
        let b_placeholder = 0x1400_0000u32.to_le_bytes();
        let relocs = [Reloc::new(0, RelocKind::AArch64Branch26, RelocTarget::Handle(batch[2]))];
        let b2 = pool.link_batch(&[LinkUnit::new(&b_placeholder, &relocs)]).unwrap();
        assert_eq!(call_u32_to_u32(pool.fn_ptr(b2[0]), 11), 40);
        b2
    };
    #[cfg(target_arch = "x86_64")]
    let wrapper = {
        let jmp_placeholder = [0xe9u8, 0, 0, 0, 0];
        let relocs = [Reloc::new(1, RelocKind::X86Rel32, RelocTarget::Handle(batch[2]))];
        let b2 = pool.link_batch(&[LinkUnit::new(&jmp_placeholder, &relocs)]).unwrap();
        assert_eq!(call_u32_to_u32(pool.fn_ptr(b2[0]), 11), 40);
        b2
    };

    // Atomic rollback on relocation error: free bytes must not leak!
    let free_before_err = pool.free_bytes();
    let bad_reloc = [Reloc::new(0, RelocKind::AArch64Branch26, RelocTarget::Address(0xffff_ffff_ffff_fffc))];
    let bad_insn = 0x1400_0000u32.to_le_bytes();
    assert!(pool.link_batch(&[LinkUnit::new(&bad_insn, &bad_reloc)]).is_err());
    assert_eq!(pool.free_bytes(), free_before_err, "failed link_batch must roll back allocations");

    pool.release_batch(&wrapper).unwrap();
    pool.release_batch(&batch).unwrap();
    assert_eq!(pool.free_bytes(), initial_free);
}

#[test]
fn jit_pool_atomic_tier_up_trampoline_executes() {
    let mut pool = JitPool::new(64 * 1024).unwrap();
    let initial_free = pool.free_bytes();

    let tier0 = pool.emit(&emit_ret_u32(111)).unwrap();
    let tier1 = pool.emit(&emit_ret_u32(222)).unwrap();
    let tier2 = pool.emit(&emit_ret_u32(333)).unwrap();
    pool.publish();

    let slot = pool.alloc_tier_slot(tier0).unwrap();
    let stub_addr = pool.tier_slot_address(slot);
    assert_eq!(stub_addr, pool.tier_slot_ptr(slot) as u64);

    // Tier 0 active.
    assert_eq!(call(stub_addr), 111);
    #[cfg(target_arch = "aarch64")]
    {
        let entry_word = u32::from_le_bytes(pool.rw_bytes(slot.handle()).unwrap()[0..4].try_into().unwrap());
        assert_eq!(entry_word & 0xfc00_0000, 0x1400_0000, "near target must use 4-byte B instruction");
    }

    // Live atomic tier-up to Tier 1 at the exact same stub address!
    pool.retarget(slot, tier1).unwrap();
    assert_eq!(call(stub_addr), 222);

    // Live atomic tier-up to Tier 2 at the exact same stub address!
    pool.retarget(slot, tier2).unwrap();
    assert_eq!(call(stub_addr), 333);

    // Live retarget to a host Rust `extern "C" fn` (exercises far > ±128 MiB indirect path on AArch64!).
    let host_a = host_tier_fn_a as *const () as usize as u64;
    let host_b = host_tier_fn_b as *const () as usize as u64;
    pool.retarget_address(slot, host_a).unwrap();
    assert_eq!(call(stub_addr), 777);
    #[cfg(target_arch = "aarch64")]
    if host_a.abs_diff(stub_addr) >= (1 << 27) {
        let entry_word = u32::from_le_bytes(pool.rw_bytes(slot.handle()).unwrap()[0..4].try_into().unwrap());
        assert_eq!(entry_word, 0xd503_201f, "far target must fall through NOP -> LDR x16; BR x16");
    }

    // Far -> Far atomic 8-byte word swap.
    pool.retarget_address(slot, host_b).unwrap();
    assert_eq!(call(stub_addr), 888);

    // Far -> Near transition back to in-slab Tier 0!
    pool.retarget(slot, tier0).unwrap();
    assert_eq!(call(stub_addr), 111);
    #[cfg(target_arch = "aarch64")]
    {
        let entry_word = u32::from_le_bytes(pool.rw_bytes(slot.handle()).unwrap()[0..4].try_into().unwrap());
        assert_eq!(entry_word & 0xfc00_0000, 0x1400_0000, "retarget back to near must restore B");
    }

    pool.release_tier_slot(slot).unwrap();
    pool.release(tier0).unwrap();
    pool.release(tier1).unwrap();
    pool.release(tier2).unwrap();
    assert_eq!(pool.free_bytes(), initial_free);
}

#[test]
fn jit_pool_concurrent_tier_up_under_load() {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let mut pool = JitPool::new(64 * 1024).unwrap();
    let t0 = pool.emit(&emit_ret_u32(10)).unwrap();
    let t1 = pool.emit(&emit_ret_u32(20)).unwrap();
    let t2 = pool.emit(&emit_ret_u32(30)).unwrap();
    pool.publish();

    let slot = pool.alloc_tier_slot(t0).unwrap();
    let stub_addr = pool.tier_slot_address(slot);
    let stop = AtomicBool::new(false);
    let seen_mask = AtomicU64::new(0);
    let started = AtomicBool::new(false);

    let iters = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut iters = 0u64;
            started.store(true, Ordering::Release);
            while !stop.load(Ordering::Relaxed) || iters < 1000 {
                let v = call(stub_addr);
                let bit = match v {
                    10 => 1u64 << 0,
                    20 => 1u64 << 1,
                    30 => 1u64 << 2,
                    777 => 1u64 << 3,
                    other => panic!("corrupted tier return value {other}"),
                };
                seen_mask.fetch_or(bit, Ordering::Relaxed);
                iters += 1;
            }
            iters
        });

        // Wait for the reader thread to enter its call loop before retargeting so on many-core
        // hosts the 200 rounds don't all finish before the reader observes a single tier.
        while !started.load(Ordering::Acquire) || seen_mask.load(Ordering::Relaxed) == 0 {
            core::hint::spin_loop();
        }
        let host_a = host_tier_fn_a as *const () as usize as u64;
        for round in 0..200 {
            match round % 4 {
                0 => pool.retarget(slot, t1).unwrap(),
                1 => pool.retarget(slot, t2).unwrap(),
                2 => pool.retarget_address(slot, host_a).unwrap(),
                _ => pool.retarget(slot, t0).unwrap(),
            }
            if round == 0 {
                let mut spins = 0u32;
                while seen_mask.load(Ordering::Relaxed).count_ones() < 2 && spins < 1_000_000 {
                    core::hint::spin_loop();
                    spins += 1;
                }
            } else {
                core::hint::spin_loop();
            }
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap()
    });
    assert!(iters >= 1000);
    // The reader must have observed at least two distinct tiers without any fault.
    assert!(seen_mask.load(Ordering::Relaxed).count_ones() >= 2);
}

#[test]
fn reloc_patcher_boundary_and_error_cases() {
    // 1. AArch64Branch26: exact ±128 MiB boundaries & B/BL preservation.
    let base = 0x1_0000_0000u64;
    let max_delta = (1u64 << 27) - 4; // +128 MiB - 4
    let min_delta = 1u64 << 27; // -128 MiB

    // BL max positive
    let mut buf = 0x9400_0000u32.to_le_bytes();
    apply_reloc(&mut buf, base, 0, RelocKind::AArch64Branch26, base + max_delta, 0).unwrap();
    assert_eq!(u32::from_le_bytes(buf), 0x95ff_ffff);

    // B min negative
    let mut buf = 0x1400_0000u32.to_le_bytes();
    apply_reloc(&mut buf, base, 0, RelocKind::AArch64Branch26, base - min_delta, 0).unwrap();
    assert_eq!(u32::from_le_bytes(buf), 0x1600_0000);

    // Out-of-range by 4 bytes (both directions)
    let mut buf = 0x1400_0000u32.to_le_bytes();
    assert!(apply_reloc(&mut buf, base, 0, RelocKind::AArch64Branch26, base + (1 << 27), 0).is_err());
    assert!(apply_reloc(&mut buf, base, 0, RelocKind::AArch64Branch26, base - min_delta - 4, 0).is_err());
    // Misaligned target
    assert!(apply_reloc(&mut buf, base, 0, RelocKind::AArch64Branch26, base + 2, 0).is_err());
    // Wrong opcode (`ret`)
    let mut ret_buf = 0xd65f_03c0u32.to_le_bytes();
    assert!(apply_reloc(&mut ret_buf, base, 0, RelocKind::AArch64Branch26, base + 16, 0).is_err());

    // 2. AArch64AdrpAdd: exact ±4 GiB page boundaries & lo12 decoding.
    let mut pair = [0u8; 8];
    let adrp_x5 = 0x9000_0005u32;
    let add_x5_x5 = 0x9100_00a5u32;
    let reset_pair = |p: &mut [u8; 8]| {
        p[0..4].copy_from_slice(&adrp_x5.to_le_bytes());
        p[4..8].copy_from_slice(&add_x5_x5.to_le_bytes());
    };
    let decode_adrp_add = |p: &[u8; 8], pc: u64| -> u64 {
        let a = u32::from_le_bytes(p[0..4].try_into().unwrap());
        let b = u32::from_le_bytes(p[4..8].try_into().unwrap());
        let immlo = (a >> 29) & 3;
        let immhi = (a >> 5) & 0x7_ffff;
        let imm21 = (immhi << 2) | immlo;
        let signed_pages = ((imm21 as i32) << 11) >> 11;
        let page_addr = ((pc & !0xfff) as i128 + (i128::from(signed_pages) << 12)) as u64;
        let lo12 = u64::from((b >> 10) & 0xfff);
        page_addr + lo12
    };

    let pc = 0x2_0000_0124u64;
    let max_target = (pc & !0xfff) + (((1u64 << 20) - 1) << 12) + 0xabc;
    reset_pair(&mut pair);
    apply_reloc(&mut pair, pc, 0, RelocKind::AArch64AdrpAdd, max_target, 0).unwrap();
    assert_eq!(decode_adrp_add(&pair, pc), max_target);

    let min_target = (pc & !0xfff) - ((1u64 << 20) << 12) + 0xdef;
    reset_pair(&mut pair);
    apply_reloc(&mut pair, pc, 0, RelocKind::AArch64AdrpAdd, min_target, 0).unwrap();
    assert_eq!(decode_adrp_add(&pair, pc), min_target);

    // One page past max / min -> error.
    reset_pair(&mut pair);
    assert!(apply_reloc(&mut pair, pc, 0, RelocKind::AArch64AdrpAdd, max_target + 0x1000, 0).is_err());
    assert!(apply_reloc(&mut pair, pc, 0, RelocKind::AArch64AdrpAdd, min_target - 0x1000, 0).is_err());
    // Invalid opcodes rejected.
    let mut bad_pair = [0u8; 8];
    assert!(apply_reloc(&mut bad_pair, pc, 0, RelocKind::AArch64AdrpAdd, pc + 0x100, 0).is_err());

    // 3. X86Rel32: exact i32 boundaries and addends.
    let mut x86 = [0u8; 5];
    x86[0] = 0xe8;
    apply_reloc(&mut x86, base, 1, RelocKind::X86Rel32, base + 5 + i32::MAX as u64, 0).unwrap();
    assert_eq!(i32::from_le_bytes(x86[1..5].try_into().unwrap()), i32::MAX);
    apply_reloc(&mut x86, base, 1, RelocKind::X86Rel32, base + 5 - (1u64 << 31), 0).unwrap();
    assert_eq!(i32::from_le_bytes(x86[1..5].try_into().unwrap()), i32::MIN);
    assert!(apply_reloc(&mut x86, base, 1, RelocKind::X86Rel32, base + 5 + (1u64 << 31), 0).is_err());
    assert!(apply_reloc(&mut x86, base, 1, RelocKind::X86Rel32, base + 4 - (1u64 << 31), 0).is_err());

    // 4. Abs64: full 64-bit address + addend + overflow rejection.
    let mut abs = [0u8; 8];
    apply_reloc(&mut abs, base, 0, RelocKind::Abs64, 0xdead_beef_cafe_0000, 0x1234).unwrap();
    assert_eq!(u64::from_le_bytes(abs), 0xdead_beef_cafe_1234);
    assert!(apply_reloc(&mut abs, base, 0, RelocKind::Abs64, u64::MAX, 1).is_err());
    assert!(apply_reloc(&mut abs, base, 0, RelocKind::Abs64, 0, -1).is_err());
    // Out-of-bounds offset.
    assert!(apply_reloc(&mut abs, base, 4, RelocKind::Abs64, base, 0).is_err());
}
