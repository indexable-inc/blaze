//! [`Executable`] runs what it was given: checked against the mapping blaze-lazy's and
//! blaze-stencil's tests each made for themselves before it (`reference`), and for the data's
//! reach from the code by a PC-relative load.

#![cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
))]

use super::Executable;

/// `mov eax, 42; ret` / `mov w0, #42; ret`.
#[cfg(target_arch = "x86_64")]
const FORTY_TWO: &[u8] = &[0xb8, 42, 0, 0, 0, 0xc3];
#[cfg(target_arch = "aarch64")]
const FORTY_TWO: &[u8] = &[0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];

/// Calls the C function `() -> u32` at `at`.
fn call(at: u64) -> u32 {
    // SAFETY: every caller passes the address of a function of this signature in the C
    // convention, mapped executable and live for the call.
    let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(at as usize) };
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
    pub(super) struct Code {
        pub(super) at: *mut u8,
        len: usize,
    }

    impl Drop for Code {
        fn drop(&mut self) {
            // SAFETY: the mapping `new` made; no code of it runs once the test drops it.
            unsafe { libc::munmap(self.at.cast(), self.len) };
        }
    }

    impl Code {
        pub(super) fn new(bytes: &[u8]) -> Code {
            let len = bytes.len().next_multiple_of(1 << 16);
            #[cfg(target_os = "macos")]
            let (protection, flags) =
                (libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT);
            #[cfg(not(target_os = "macos"))]
            let (protection, flags) = (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
            // SAFETY: an anonymous mapping at no fixed address; the result is checked.
            let at = unsafe { libc::mmap(std::ptr::null_mut(), len, protection, flags, -1, 0) };
            assert_ne!(at, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
            let code = Code { at: at.cast(), len };
            #[cfg(target_os = "macos")]
            // SAFETY: this thread's view of MAP_JIT pages writable, until turned back below.
            unsafe {
                libc::pthread_jit_write_protect_np(0)
            };
            // SAFETY: the fresh mapping of `len >= bytes.len()` writable bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), code.at, bytes.len()) };
            #[cfg(target_os = "macos")]
            // SAFETY: executable again on this thread; then the instruction cache sees the bytes.
            unsafe { libc::pthread_jit_write_protect_np(1) };
            #[cfg(target_os = "macos")]
            // SAFETY: the instruction cache sees the bytes in the fresh mapping.
            unsafe { sys_icache_invalidate(code.at.cast(), bytes.len()) };
            #[cfg(not(target_os = "macos"))]
            {
                // SAFETY: the whole mapping made above.
                let sealed = unsafe { libc::mprotect(code.at.cast(), len, libc::PROT_READ | libc::PROT_EXEC) };
                assert_eq!(sealed, 0, "mprotect: {}", std::io::Error::last_os_error());
                #[cfg(target_arch = "aarch64")]
                icache(code.at as usize, bytes.len());
            }
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
    assert_eq!(code_at(exec.code_address(), FORTY_TWO.len()), code_at(reference.at as u64, FORTY_TWO.len()));
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
    let mut exec = Executable::written(16, 3 << 16, |code, data| load_first_word(code, data)).unwrap();
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

/// The thread's `MAP_JIT` pages are executable again after a `write` that panicked.
#[test]
fn a_write_that_panics_leaves_the_thread_running_code() {
    let first = Executable::new(FORTY_TWO, 0).unwrap();
    let panicked = std::panic::catch_unwind(|| Executable::written(16, 16, |_, _| panic!("a write that fails")));
    assert!(panicked.is_err());
    assert_eq!(call(first.code_address()), 42);
    assert_eq!(call(Executable::new(FORTY_TWO, 0).unwrap().code_address()), 42);
}
