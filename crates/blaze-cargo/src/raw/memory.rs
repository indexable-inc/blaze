//! Memory: what a process holds (macOS and Linux) and the machine has (macOS), the kernel's
//! fatal limit on this process's footprint (macOS), and the allocator's purge.

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

// SAFETY: libsystem_kernel's `int memorystatus_control(uint32_t command, int32_t pid, uint32_t
// flags, void *buffer, size_t buffersize)`, as XNU's bsd/sys/kern_memorystatus.h declares it
// (apple-oss-distributions/xnu, main at f6217f89, line 333); a declaration that differs would
// have every call pass its arguments wrongly.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    // Private (rule macos-private-apis): no public API sets a fatal limit on a process's
    // physical footprint. With one the kernel kills the daemon the moment it passes its
    // ceiling, where otherwise the Mac pages itself into a full compressor and a watchdog
    // panic (2026-11-05: twice, on a 128 GiB Mac). A daemon's own watch reads the footprint
    // only every 100 ms; the kernel's ledger holds it at every page it takes.
    fn memorystatus_control(
        command: u32,
        pid: i32,
        flags: u32,
        buffer: *mut libc::c_void,
        buffersize: usize,
    ) -> libc::c_int;
}

/// `MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES` of XNU's bsd/sys/kern_memorystatus.h (line 344):
/// sets a process's active and inactive memory limits and their attributes apart.
#[cfg(target_os = "macos")]
const MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES: u32 = 7;

/// `MEMORYSTATUS_MEMLIMIT_ATTR_FATAL` of kern_memorystatus.h (line 495): passing the limit
/// kills the process (`memorystatus_on_ledger_footprint_exceeded`'s fatal path, a SIGKILL with
/// the jetsam reason `JETSAM_REASON_MEMORY_PERPROCESSLIMIT`), not only warns it.
#[cfg(target_os = "macos")]
const MEMORYSTATUS_MEMLIMIT_ATTR_FATAL: u32 = 0x1;

/// `memorystatus_memlimit_properties_t` of kern_memorystatus.h (lines 482-487), field for field:
/// each limit in MiB (the kernel's "MB", times 1024 * 1024), which the kernel holds the
/// process's physical footprint to ([`footprint`]'s `ri_phys_footprint`).
#[cfg(target_os = "macos")]
#[repr(C)]
struct MemlimitProperties {
    memlimit_active: i32,
    memlimit_active_attr: u32,
    memlimit_inactive: i32,
    memlimit_inactive_attr: u32,
}

/// Has the kernel kill this process once its physical footprint passes `bytes` (rounded down to
/// whole MiB, at least one), active or inactive alike: `memorystatus_control(
/// MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES, getpid(), 0, &props, sizeof props)` with
/// `MEMORYSTATUS_MEMLIMIT_ATTR_FATAL` on both limits. The MiB the limit is at.
///
/// XNU's `memorystatus_control` (bsd/kern/kern_memorystatus.c, line 9193) refuses this command
/// with `EPERM` to a process that is neither root nor entitled `com.apple.private.memorystatus`,
/// its own pid included (only the freeze commands, and on development kernels the limit's raise
/// and re-arm on its own pid, skip that check), and the sandbox's MAC hook may refuse it too.
/// The error is the caller's to say: its daemon is then held by its own watch alone.
#[cfg(target_os = "macos")]
pub fn hold_own_footprint(bytes: u64) -> std::io::Result<i32> {
    let mib = i32::try_from(bytes >> 20).unwrap_or(i32::MAX).max(1);
    let mut props = MemlimitProperties {
        memlimit_active: mib,
        memlimit_active_attr: MEMORYSTATUS_MEMLIMIT_ATTR_FATAL,
        memlimit_inactive: mib,
        memlimit_inactive_attr: MEMORYSTATUS_MEMLIMIT_ATTR_FATAL,
    };
    // SAFETY: `getpid` takes nothing and touches no memory of this process.
    let pid = unsafe { libc::getpid() };
    // SAFETY: `props` is a live, whole `memorystatus_memlimit_properties_t` (`#[repr(C)]`, the
    // header's four fields in order) for the call, which copies `size_of` of it in and keeps no
    // pointer to it; a size other than the struct's is refused (`EINVAL`), never read past.
    let set = unsafe {
        memorystatus_control(
            MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES,
            pid,
            0,
            (&raw mut props).cast(),
            size_of::<MemlimitProperties>(),
        )
    };
    match set {
        0 => Ok(mib),
        _ => Err(std::io::Error::last_os_error()),
    }
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

// SAFETY: libsystem_malloc's `size_t malloc_zone_pressure_relief(malloc_zone_t *zone, size_t
// goal)`, as <malloc/malloc.h> declares it (macOS 10.7 on): a null zone is every zone, a goal of
// 0 is as much as each can give.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    // Undocumented (rule macos-private-apis): declared in <malloc/malloc.h> with no man page.
    // The system zones (nano and scalable, which std, the C libraries blaze links and every
    // `malloc` outside mimalloc use) keep freed pages until memory is short; this has them
    // madvise those pages away now, which no documented call does. A purge before the plane
    // refuses work or the guard ends the daemon (docs/v2/plan/admission-by-footprint.md).
    fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
}

/// Has every malloc zone of the system give back the freed pages it keeps; the bytes they
/// gave. The system's allocator only: mimalloc's are [`purge_freed_pages`]'.
#[cfg(target_os = "macos")]
pub fn relieve_system_zones() -> u64 {
    // SAFETY: a null zone and a goal of 0 are the documented "every zone, all it can"; the call
    // takes each zone's lock itself and touches no memory of the caller's.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) as u64 }
}

/// Gives back every freed page the process keeps that a purge can reach: each thread's
/// mimalloc heaps on its next settle (`blaze_mem::tag::give_back_heap`) and the calling thread's
/// now with what was abandoned (`mi_collect`), the warm chunks `blaze_mem` caches
/// (`blaze_mem::chunks::trim`), and on macOS the system's malloc zones. On Linux each of these
/// is `madvise(MADV_DONTNEED)` on the free spans, so the resident set falls at once. What admission
/// does before it refuses work, and the guard before it ends the daemon at its ceiling
/// (docs/v2/plan/admission-by-footprint.md).
pub fn purge() {
    let _span = tracing::info_span!(target: "blaze", "memory purge").entered();
    blaze_mem::tag::give_back_heap();
    #[cfg(feature = "native")]
    purge_freed_pages();
    blaze_mem::chunks::trim();
    #[cfg(target_os = "macos")]
    relieve_system_zones();
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

    /// The daemon's hard ceiling on macOS rests on the private `memorystatus_control` (rule
    /// macos-private-apis): an update of macOS that takes it out of libSystem fails here, by
    /// name, not in a daemon that then holds no kernel limit.
    #[cfg(target_os = "macos")]
    #[test]
    fn memorystatus_control_is_in_libsystem() {
        // SAFETY: `dlsym` reads the nul-terminated name and returns an address or null; nothing
        // is called through it.
        let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"memorystatus_control".as_ptr()) };
        assert!(
            !found.is_null(),
            "libSystem has no `memorystatus_control`, which the daemon's hard memory ceiling calls \
             (MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES)"
        );
    }

    /// The purge before a refusal or the ceiling rests on the undocumented
    /// `malloc_zone_pressure_relief` (rule macos-private-apis): a macOS without it fails here,
    /// by name, not in a daemon whose purge then links nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn malloc_zone_pressure_relief_is_in_libsystem() {
        // SAFETY: `dlsym` reads the nul-terminated name and returns an address or null; nothing
        // is called through it.
        let found =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"malloc_zone_pressure_relief".as_ptr()) };
        assert!(
            !found.is_null(),
            "libSystem has no `malloc_zone_pressure_relief`, which the daemon's memory purge calls"
        );
        super::relieve_system_zones();
    }

    /// The limit's record is the kernel's `memorystatus_memlimit_properties_t`: four 32-bit
    /// fields, 16 bytes, which the kernel refuses at any other size.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_memlimit_record_is_the_kernels_size() {
        assert_eq!(size_of::<super::MemlimitProperties>(), 16);
        assert_eq!(align_of::<super::MemlimitProperties>(), 4);
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
