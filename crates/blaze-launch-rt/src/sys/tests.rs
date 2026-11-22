//! [`Code`] on the machine: code written through the write view runs through the run view,
//! is rewritten and runs again; the data after the code is in PC-relative reach; a decommit
//! gives the memory back and keeps the rest running; a forked child runs its code and holds
//! no write view.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use super::{Code, page};

/// The code of `extern "C" fn() -> u32 { n }`: `mov w0, #n; ret`.
#[cfg(target_arch = "aarch64")]
fn returns(n: u16) -> Vec<u8> {
    let mov = 0x5280_0000u32 | (u32::from(n) << 5);
    let ret = 0xd65f_03c0u32;
    [mov.to_le_bytes(), ret.to_le_bytes()].concat()
}

/// The code of `extern "C" fn() -> u32 { n }`: `mov eax, n; ret`.
#[cfg(target_arch = "x86_64")]
fn returns(n: u16) -> Vec<u8> {
    let mut code = vec![0xb8];
    code.extend_from_slice(&u32::from(n).to_le_bytes());
    code.push(0xc3);
    code
}

/// The code, placed at `at`, of a function returning the `u32` at `data`, which it reaches
/// PC-relatively.
fn loads(at: usize, data: usize) -> Vec<u8> {
    let (at, data) = (at as i64, data as i64);
    #[cfg(target_arch = "x86_64")]
    {
        // mov eax, [rip + disp32]; ret
        let disp = i32::try_from(data - (at + 6)).expect("the data in reach");
        let mut code = vec![0x8b, 0x05];
        code.extend_from_slice(&disp.to_le_bytes());
        code.push(0xc3);
        code
    }
    #[cfg(target_arch = "aarch64")]
    {
        // adrp x1, data; ldr w0, [x1, #lo12]; ret
        let pages = (data >> 12) - (at >> 12);
        assert!((-(1 << 20)..1 << 20).contains(&pages), "the data in reach");
        let pages = pages as u32;
        let adrp = 0x9000_0001 | ((pages & 3) << 29) | (((pages >> 2) & 0x7_ffff) << 5);
        let lo12 = (data & 0xfff) as u32;
        let ldr = 0xb940_0020 | ((lo12 / 4) << 10);
        [adrp, ldr, 0xd65f_03c0].into_iter().flat_map(u32::to_le_bytes).collect()
    }
}

/// Calls the function at `at`.
fn call(at: usize) -> u32 {
    // SAFETY: every caller passes the run-view address of code this file wrote, a function
    // `extern "C" fn() -> u32`, in a mapping that is never unmapped (`Code`).
    let function: extern "C" fn() -> u32 = unsafe { core::mem::transmute(at) };
    function()
}

#[test]
fn code_written_through_one_view_runs_through_the_other_and_is_rewritten() {
    let page = page().expect("a page size");
    let mut code = Code::map(page).expect("a mapping");
    assert_ne!(code.write.as_ptr() as usize, code.base(), "two views, at two addresses");
    code.write(0, &returns(42)).expect("the first body");
    assert_eq!(call(code.base()), 42);
    // Rewritten in place, after it ran: the cache maintenance of `write` is what makes the
    // second call run the new instructions.
    code.write(0, &returns(7)).expect("the second body");
    assert_eq!(call(code.base()), 7);
    // A word stored through the write view is read through the run view.
    code.store(64, 0x5eed).expect("a word");
    // SAFETY: offset 64 of the run view, mapped readable, 8-aligned, never unmapped.
    let word = unsafe { core::ptr::read_volatile((code.base() + 64) as *const u64) };
    assert_eq!(word, 0x5eed);
}

/// No `mprotect` makes the run view writable or the write view executable: their maximum
/// protection was lowered when they were made.
#[cfg(target_os = "macos")]
#[test]
fn neither_view_can_gain_the_others_protection() {
    use blaze_sys::posix::io::{Prot, mprotect};

    let page = page().expect("a page size");
    let code = Code::map(page).expect("a mapping");
    let (run, write) = (code.exec, code.write);
    let rw = Prot::READ | Prot::WRITE;
    let rx = Prot::READ | Prot::EXEC;
    for (view, protection, what) in [
        (run, rw, "the run view writable"),
        (run, rw | Prot::EXEC, "the run view writable and executable"),
        (write, rx, "the write view executable"),
        (write, rw | Prot::EXEC, "the write view writable and executable"),
    ] {
        // SAFETY: a protection change of a page of `code`'s own views, which the kernel is
        // expected to refuse; no reference needs a permission the call could take away.
        let changed = unsafe { mprotect(view, page, protection) };
        assert!(changed.is_err(), "mprotect made {what}");
    }
}

#[test]
fn the_data_follows_the_code_in_pc_relative_reach() {
    let page = page().expect("a page size");
    let mut code = Code::map_with_data(page, 2 * page).expect("a mapping");
    assert_eq!(code.data_base(), code.base() + page);
    assert_eq!(code.data_len(), 2 * page);
    let at = code.data_base() + page + 12;
    code.write(0, &loads(code.base(), at)).expect("the load");
    assert_eq!(call(code.base()), 0, "the data starts zeroed");
    // SAFETY: `at` is 4-aligned and inside the data, which is readable and writable and never
    // unmapped; the only reader is the call below, on this thread.
    unsafe { core::ptr::write_volatile(at as *mut u32, 0x5eed_f00d) };
    assert_eq!(call(code.base()), 0x5eed_f00d);
    assert!(code.write(page, &[0; 4]).is_err(), "the data is not code: a write is refused");
}

#[test]
fn a_decommitted_page_leaves_the_others_running() {
    let page = page().expect("a page size");
    let mut code = Code::map(3 * page).expect("a mapping");
    code.write(0, &returns(1)).expect("the first page's body");
    code.write(page, &returns(2)).expect("the second page's body");
    code.write(2 * page, &returns(3)).expect("the third page's body");
    code.decommit(page, page).expect("the second page given back");
    assert_eq!((call(code.base()), call(code.base() + 2 * page)), (1, 3));
    assert!(code.decommit(page / 2, page).is_err(), "a decommit of part of a page");
}

/// The process's footprint, as macOS counts it (`TASK_VM_INFO`'s `phys_footprint`).
#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    unsafe extern "C" {
        /// `<mach/task.h>`.
        fn task_info(task: u32, flavor: u32, out: *mut i32, count: *mut u32) -> i32;
    }
    const TASK_VM_INFO: u32 = 22;
    // `task_vm_info_data_t` up to `phys_footprint` (revision 1): 152 bytes, the footprint at
    // byte 144 (`offsetof`, from the SDK's header).
    let mut info = [0u64; 19];
    let mut count = 38u32;
    // SAFETY: `info` has `count` writable 4-byte units, which the call fills at most.
    let asked = unsafe { task_info(super::macos::task(), TASK_VM_INFO, info.as_mut_ptr().cast(), &raw mut count) };
    assert_eq!(asked, 0, "task_info(TASK_VM_INFO)");
    info[18]
}

/// The cause test of `Code::decommit` on macOS: pages written through the write view count in
/// the footprint, and stop counting once decommitted (what `madvise` on two views did not do).
#[cfg(target_os = "macos")]
#[test]
fn a_decommit_takes_its_pages_out_of_the_footprint() {
    let page = page().expect("a page size");
    let len = 64 << 20;
    let mut code = Code::map(len).expect("a mapping");
    let before = footprint();
    for at in (0..len).step_by(page) {
        code.write(at, &[1]).expect("a byte of each page");
    }
    let written = footprint();
    code.decommit(0, len).expect("given back");
    let after = footprint();
    let mib = |bytes: u64| bytes >> 20;
    assert!(
        written.saturating_sub(before) >= 48 << 20,
        "64 MiB written counted {} MiB",
        mib(written.saturating_sub(before))
    );
    assert!(
        after.saturating_sub(before) <= 8 << 20,
        "64 MiB decommitted still count {} MiB of the {} MiB written",
        mib(after.saturating_sub(before)),
        mib(written.saturating_sub(before))
    );
}

/// The fork rule: a child forked from the program runs its code at the same address, with no
/// step after the fork, and holds no write view. The parent rewrites the code after the fork:
/// a macOS child runs its snapshot (the run view is inherited as a copy), a Linux child the
/// parent's new code (the memfd's pages are shared).
#[test]
fn a_forked_child_runs_its_code_and_holds_no_write_view() {
    use blaze_sys::posix::{io, proc, signal};

    let page = page().expect("a page size");
    let mut code = Code::map(page).expect("a mapping");
    code.write(0, &returns(42)).expect("the body");
    let (run, write) = (code.base(), code.write.as_ptr() as usize);
    let (reader, writer) = io::pipe().expect("pipe");
    #[cfg(target_os = "macos")]
    let pipe = [reader.fd().raw(), writer.fd().raw()];
    // SAFETY: the child only reads a byte, probes the mapping, calls code that allocates
    // nothing and takes no lock, then exits without running destructors.
    #[cfg(target_os = "macos")]
    let pid = unsafe { proc::fork() };
    // SAFETY: `fork` in a process with other threads: the child only reads a byte, asks the
    // kernel of a range, calls code that allocates nothing and takes no lock, and `_exit`s.
    #[cfg(target_os = "linux")]
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        let mut byte = 0u8;
        #[cfg(target_os = "linux")]
        let _ = blaze_sys::linux::read(reader.fd(), core::slice::from_mut(&mut byte));
        // SAFETY: one byte into a live local, from the pipe's read end.
        #[cfg(target_os = "macos")]
        unsafe { libc::read(pipe[0], (&raw mut byte).cast(), 1) };
        // SAFETY: asks whether `write..write + page` is mapped; it touches no byte.
        if unsafe { libc::msync(write as *mut core::ffi::c_void, page, io::MsyncFlags::ASYNC.bits() as i32) } == 0 {
            signal::exit_now(200);
        }
        let got = call(run);
        signal::exit_now(got as i32);
    }
    code.write(0, &returns(7)).expect("the parent's new body");
    assert_eq!(io::write(writer.fd(), &[0]), Ok(1), "write to the pipe");
    let (waited, proc::WaitStatus(status)) = proc::wait_pid(pid, proc::WaitFlags::NONE)
        .unwrap_or_else(|err| panic!("waitpid: {}", std::io::Error::from_raw_os_error(err.raw().into())))
        .expect("waitpid with no WNOHANG returns a child");
    assert_eq!(waited, pid, "waitpid");
    for end in [reader, writer] {
        let _ = io::close(end);
    }
    let expected = if cfg!(target_os = "macos") { 42 } else { 7 };
    assert!(
        proc::WIFEXITED(status) && proc::WEXITSTATUS(status) == expected,
        "the child ended with status {status:#x} (signal {}; 200 is a write view it should not \
         hold), not by returning {expected} from its code",
        if proc::WIFSIGNALED(status) { proc::WTERMSIG(status) } else { 0 },
    );
    assert_eq!(call(run), 7, "the parent runs its new body");
}
