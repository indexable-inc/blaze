//! Memory: what a process holds (macOS and Linux) and the machine has (macOS), and the
//! allocator's purge.

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

/// Bytes of memory a process holds on Linux: its resident set, `VmRSS` in
/// `/proc/<pid>/status`, which (unlike macOS's footprint) leaves out pages swapped out.
#[cfg(target_os = "linux")]
pub fn footprint(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?; // alloc: one read per plot
    resident(&status)
}

/// The `VmRSS` line of a `/proc/<pid>/status`, in bytes.
#[cfg(any(target_os = "linux", test))]
fn resident(status: &str) -> Option<u64> {
    let kib = status.lines().find_map(|it| it.strip_prefix("VmRSS:"))?.trim().strip_suffix("kB")?;
    kib.trim().parse::<u64>().ok()?.checked_mul(1024)
}

/// Bytes of memory this process holds as the system counts it, what a limit on it is held to
/// (`blaze_mem::tag::resident_by`): its physical footprint on macOS ([`footprint`]), its
/// resident set on Linux from `/proc/self/statm`. Allocates nothing (the path fits std's stack
/// buffer and the file is read into one of its own), as it is read once a run ends.
#[cfg(target_os = "macos")]
pub fn own_footprint() -> Option<u64> {
    footprint(std::process::id())
}

/// Bytes of memory this process holds on Linux: the second field of `/proc/self/statm`, its
/// resident pages, times the page size. Allocates nothing.
#[cfg(target_os = "linux")]
pub fn own_footprint() -> Option<u64> {
    use std::io::Read as _;
    let mut buf = [0u8; 256];
    let mut file = std::fs::File::open("/proc/self/statm").ok()?;
    let len = file.read(&mut buf).ok()?;
    let pages = statm_resident(buf.get(..len)?)?;
    // SAFETY: sysconf reads a constant of the system; no memory of ours.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages.checked_mul(u64::try_from(page).ok()?)
}

/// The resident pages of a `/proc/<pid>/statm`: its second field.
#[cfg(any(target_os = "linux", test))]
fn statm_resident(statm: &[u8]) -> Option<u64> {
    let field = statm.split(|&it| it == b' ').nth(1)?;
    std::str::from_utf8(field).ok()?.trim().parse().ok()
}

/// No reading of the process's footprint here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn own_footprint() -> Option<u64> {
    None
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
/// otherwise. Only where blaze names its allocator, mimalloc (`native`); the seed profile runs
/// on std's, which has no such call.
#[cfg(feature = "native")]
pub fn purge_freed_pages() {
    // SAFETY: `mi_collect` reads and writes no memory of the caller's, and may be called from
    // any thread at any time.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_holds_some() {
        assert!(super::footprint(std::process::id()).is_some_and(|it| it > 0));
    }

    #[test]
    fn the_resident_set_is_read_from_its_line_in_kib() {
        let status = "Name:\tblaze\nVmPeak:\t  9000 kB\nVmRSS:\t   1234 kB\nThreads:\t4\n";
        assert_eq!(super::resident(status), Some(1234 * 1024));
        assert_eq!(super::resident("Name:\tkthreadd\nThreads:\t1\n"), None);
    }

    #[test]
    fn statm_says_its_resident_pages_second() {
        assert_eq!(super::statm_resident(b"52345 1234 567 89 0 4321 0\n"), Some(1234));
        assert_eq!(super::statm_resident(b"52345"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn this_process_holds_some_by_its_own_count() {
        assert!(super::own_footprint().is_some_and(|it| it > 0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_and_the_machine_hold_some() {
        let held = super::footprint(std::process::id());
        assert!(held.is_some_and(|it| it > 0));
        assert!(super::machine_bytes().is_some_and(|it| it > held.unwrap()));
    }

    #[cfg(feature = "native")]
    #[test]
    fn a_purge_leaves_what_is_live() {
        let kept = vec![7u8; 1 << 20];
        drop(vec![0u8; 16 << 20]);
        super::purge_freed_pages();
        assert!(kept.iter().all(|&it| it == 7));
    }
}
