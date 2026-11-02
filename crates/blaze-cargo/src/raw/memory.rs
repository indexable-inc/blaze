//! Memory: what a process holds and the machine has on macOS, the allocator's purge, and,
//! under `heap-profile`, jemalloc's controls that its safe API (`tikv_jemalloc_ctl`'s
//! `Access`) does not reach: a purge, which takes no value, and a profile dump, whose safe
//! form takes only a `&'static` path (one leaked per dump).

/// Bytes of memory a process of this user owns: its physical footprint, which is what
/// Activity Monitor shows, and which still counts pages macOS has compressed.
#[cfg(target_os = "macos")]
pub fn footprint(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `info` is a live, writable `rusage_info_v4`, the record `RUSAGE_INFO_V4` has the
    // call write; another flavor would be a record of another size, written past its end.
    let ok = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    if ok != 0 {
        return None;
    }
    // SAFETY: the record is integers alone, for which the zeros `info` began as are a value,
    // and the call wrote another over them.
    Some(unsafe { info.assume_init() }.ri_phys_footprint)
}

/// Bytes of memory the machine has.
#[cfg(target_os = "macos")]
pub fn machine_bytes() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: the name is a C string; `hw.memsize` is a 64-bit integer, and `bytes` is a live
    // `u64` whose size `size` says, so the call writes no further than it. A `size` larger than
    // `bytes` would have it write past its end.
    let ok = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (ok == 0 && bytes != 0).then_some(bytes)
}

/// Gives the system back the pages of what was freed, which the allocator keeps to reuse
/// otherwise.
#[cfg(not(feature = "heap-profile"))]
pub fn purge_freed_pages() {
    // SAFETY: `mi_collect` reads and writes no memory of the caller's, and may be called from
    // any thread at any time.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

/// Gives the system back the pages of what was freed, which jemalloc keeps for ten seconds
/// to reuse otherwise.
#[cfg(feature = "heap-profile")]
pub fn purge_freed_pages() {
    use std::ptr::null_mut;
    // SAFETY: the name is a C string, and `arena.<i>.purge` reads and writes no value, which
    // the null pointers and zero length say; 4096 (`MALLCTL_ARENAS_ALL`) is every arena. A
    // control that took a value would be handed none and refuse.
    unsafe {
        tikv_jemalloc_sys::mallctl(
            c"arena.4096.purge".as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        )
    };
}

/// Where the program itself is loaded, which a heap profile's addresses are relative to.
#[cfg(all(feature = "heap-profile", target_os = "macos"))]
pub fn program_image_base() -> usize {
    // SAFETY: image 0 is the program itself, which is loaded for as long as it runs, and
    // only its address is kept; an index past the images loaded would be answered with null.
    let header = unsafe { libc::_dyld_get_image_header(0) };
    header as usize
}

/// Has jemalloc write a profile of the heap to `path`.
#[cfg(feature = "heap-profile")]
pub fn dump_heap_profile(path: &std::ffi::CStr) -> tikv_jemalloc_ctl::Result<()> {
    // SAFETY: `prof.dump` takes a pointer to a C string, which `path` is for the length of the
    // call, and keeps nothing of it after; a value of another type would be read as a path.
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", path.as_ptr()) }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_and_the_machine_hold_some() {
        let held = super::footprint(std::process::id());
        assert!(held.is_some_and(|it| it > 0));
        assert!(super::machine_bytes().is_some_and(|it| it > held.unwrap()));
    }

    #[test]
    fn a_purge_leaves_what_is_live() {
        let kept = vec![7u8; 1 << 20];
        drop(vec![0u8; 16 << 20]);
        super::purge_freed_pages();
        assert!(kept.iter().all(|&it| it == 7));
    }
}
