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
/// (`blaze_mem::tag::resident_by`): its physical footprint on macOS ([`footprint`]). Allocates
/// nothing, as it is read once a run ends.
#[cfg(target_os = "macos")]
pub fn own_footprint() -> Option<u64> {
    footprint(std::process::id())
}

/// The daemon's own cgroup's `memory.current` and `memory.stat`, once the kernel holds its
/// ceiling there ([`count_own_cgroup`]).
#[cfg(target_os = "linux")]
static OWN_CGROUP: std::sync::OnceLock<(std::path::PathBuf, std::path::PathBuf)> =
    std::sync::OnceLock::new();

/// From here on [`own_footprint`] is the charge of the cgroup at `dir`: the daemon's own, whose
/// `memory.max` the kernel now holds its ceiling with (`serve::ceiling`). The first call wins.
#[cfg(target_os = "linux")]
pub fn count_own_cgroup(dir: &std::path::Path) {
    // alloc: once a ceiling is set, the two paths
    _ = OWN_CGROUP.set((dir.join("memory.current"), dir.join("memory.stat")));
}

/// Bytes of memory the kernel holds this daemon to on Linux (rule `one-memory-measure`):
///
/// - where the daemon is in a cgroup of its own with a limit ([`count_own_cgroup`]), that
///   cgroup's charge the kernel cannot take back on its own ([`charged`]): `memory.current`
///   less its file cache. That is every process of the cgroup, the kernel memory charged to
///   it and shared memory, which `memory.max` counts and a resident set leaves out;
/// - elsewhere (no cgroup v2 delegated: the daemon's own watch is the bound) the process's
///   resident set, the second field of `/proc/self/statm` times the page size.
///
/// A cgroup that cannot be read is `None`, never the resident set in its place. Allocates
/// nothing: each path fits std's stack buffer and each file is read into a buffer here.
#[cfg(target_os = "linux")]
pub fn own_footprint() -> Option<u64> {
    match OWN_CGROUP.get() {
        Some((current, stat)) => cgroup_charge(current, stat),
        None => own_resident(),
    }
}

/// This process's resident set, from `/proc/self/statm`.
#[cfg(target_os = "linux")]
fn own_resident() -> Option<u64> {
    let mut buf = [0u8; 256];
    let pages = statm_resident(read_into(std::path::Path::new("/proc/self/statm"), &mut buf)?)?;
    // SAFETY: sysconf reads a constant of the system; no memory of ours.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages.checked_mul(u64::try_from(page).ok()?)
}

/// [`charged`] of the cgroup whose `memory.current` and `memory.stat` these are.
#[cfg(any(target_os = "linux", test))]
fn cgroup_charge(current: &std::path::Path, stat: &std::path::Path) -> Option<u64> {
    let mut buf = [0u8; 64];
    let current = decimal(read_into(current, &mut buf)?)?;
    // `file` and `shmem` are among `memory.stat`'s first ten lines; the whole file is under
    // 4 KiB on Linux 7.2.
    let mut buf = [0u8; 8192];
    charged(current, read_into(stat, &mut buf)?)
}

/// What a cgroup is charged that the kernel cannot reclaim without stopping it: `current`
/// (`memory.current`) less the file cache of `stat` (`memory.stat`'s `file`), which the kernel
/// drops before `memory.max` kills, except the part of it that is shared memory and tmpfs
/// (`shmem`), which with no swap stays. `None` when `stat` lacks either line.
#[cfg(any(target_os = "linux", test))]
fn charged(current: u64, stat: &[u8]) -> Option<u64> {
    let field = |name: &[u8]| {
        stat.split(|&it| it == b'\n')
            .find_map(|line| decimal(line.strip_prefix(name)?.strip_prefix(b" ")?))
    };
    let (file, shmem) = (field(b"file")?, field(b"shmem")?);
    Some(current.saturating_sub(file.saturating_sub(shmem)))
}

/// A decimal number with space around it, as a cgroup file and a `/proc` field hold one.
#[cfg(any(target_os = "linux", test))]
fn decimal(text: &[u8]) -> Option<u64> {
    std::str::from_utf8(text).ok()?.trim().parse().ok()
}

/// The start of the file at `path`, as much as `buf` holds, without allocating.
#[cfg(any(target_os = "linux", test))]
fn read_into<'a>(path: &std::path::Path, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut len = 0;
    // Bounded by the buffer: each turn reads at least a byte or ends.
    while len < buf.len() {
        match file.read(&mut buf[len..]).ok()? {
            0 => break,
            read => len += read,
        }
    }
    buf.get(..len)
}

/// The resident pages of a `/proc/<pid>/statm`: its second field.
#[cfg(any(target_os = "linux", test))]
fn statm_resident(statm: &[u8]) -> Option<u64> {
    decimal(statm.split(|&it| it == b' ').nth(1)?)
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

/// The most a purge waits for every pool worker's heap give-back before it reads the footprint
/// ([`purge`]): a worker gives back as it next allocates for a task or goes idle, and an idle
/// one is woken for it, so a quarter of a second covers every worker but one that runs code
/// allocating nothing of the global heap (LLVM's, on the system's malloc), which the reading
/// then still holds and the purge's span names (`gave` of `of`).
pub const SETTLE_WITHIN: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a purge looks whether every worker gave back, as it waits.
const SETTLE_LOOK: std::time::Duration = std::time::Duration::from_millis(1);

/// What one [`purge`] did: the footprint before it and after (`None` where the system does not
/// say), and whether every worker had given back its heaps' free pages before the second
/// reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Purged {
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub settle: blaze_mem::tag::Settle,
}

impl Purged {
    /// The bytes the footprint fell by: what the purge returned, less what grew meanwhile.
    pub fn returned(&self) -> u64 {
        self.before.zip(self.after).map_or(0, |(before, after)| before.saturating_sub(after))
    }
}

/// The bytes every purge so far returned ([`Purged::returned`]): the `allocator purged bytes`
/// counter.
static PURGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The bytes every purge of this process returned so far, summed ([`purge`]).
pub fn purged_bytes() -> u64 {
    PURGED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Gives back every freed page the process keeps that a purge can reach, then reads the
/// footprint once they are out of it, and says what it returned:
///
/// 1. Asks every thread of the global allocator to give back what its mimalloc heaps keep and
///    wakes the idle pool workers for it (`blaze_mem::tag::ask_heap_give_back`). The pages a
///    thread's heaps allocate from or retired empty, and the blocks other threads freed into
///    them, are that thread's alone to collect (mimalloc 3's `mi_heap_collect` collects only
///    the calling thread's theap), so the purge cannot do it for them.
/// 2. Waits, at most [`SETTLE_WITHIN`], until every pool worker has
///    (`blaze_mem::tag::heap_give_back_settled`). A purge that read the footprint at once,
///    as this one did before 2026-11-09, read it with all they kept still in it: the daemon
///    then stopped at its 47 GiB ceiling with up to 7.5 GiB of the heap freed and resident.
/// 3. Purges mimalloc's arenas, every one, now (`mi_collect(true)`: forced, so not after
///    `purge_delay`, and visiting every arena). A worker's own forced purge skips them all
///    while another thread holds mimalloc's purge guard (arena.c's `mi_atomic_guard`), so
///    this one, after every worker, is what reaches each page they gave back. mimalloc
///    decommits a purged range with `MADV_FREE_REUSABLE` on macOS (prim.c's
///    `_mi_prim_decommit`), which takes it out of the footprint at once, where `MADV_FREE`
///    leaves it counted until the kernel takes it; `MADV_DONTNEED` on Linux.
/// 4. Purges the warm chunks `blaze_mem` caches (`blaze_mem::chunks::trim`, the same
///    `MADV_FREE_REUSABLE`) and on macOS the system's malloc zones.
/// 5. Reads the footprint again.
///
/// What admission asks before it refuses work (on [`purger`]'s thread, never the loop's), and
/// the guard before it ends the daemon at its ceiling (docs/memory-footprint.md, step 2).
pub fn purge() -> Purged {
    let span = tracing::info_span!(
        target: "blaze",
        "memory purge",
        before = tracing::field::Empty,
        after = tracing::field::Empty,
        returned = tracing::field::Empty,
        gave = tracing::field::Empty,
        of = tracing::field::Empty,
    )
    .entered();
    let before = own_footprint();
    blaze_mem::tag::ask_heap_give_back();
    let settle = settle_heaps();
    #[cfg(feature = "native")]
    purge_freed_pages();
    blaze_mem::chunks::trim();
    #[cfg(target_os = "macos")]
    relieve_system_zones();
    let after = own_footprint();
    let purged = Purged { before, after, settle };
    PURGED.fetch_add(purged.returned(), std::sync::atomic::Ordering::Relaxed);
    if let Some(before) = before {
        span.record("before", before);
    }
    if let Some(after) = after {
        span.record("after", after);
    }
    span.record("returned", purged.returned());
    if let blaze_mem::tag::Settle::Pending { gave, of } = settle {
        span.record("gave", gave);
        span.record("of", of);
    }
    purged
}

/// Waits until every pool worker gave back for the give-back asked now, or [`SETTLE_WITHIN`]
/// passes: looks every [`SETTLE_LOOK`], under a span naming the wait (rule
/// every-wait-says-what), on the deadline's clock (rules bounded-progress, deadline-waits).
/// Where it stood as it stopped.
fn settle_heaps() -> blaze_mem::tag::Settle {
    use blaze_core::wait::{Clock as _, Deadline, Monotonic};
    use blaze_mem::tag::Settle;
    let _span =
        tracing::info_span!(target: "blaze", "memory purge waits for each worker's heaps")
            .entered();
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), SETTLE_WITHIN);
    loop {
        let settle = blaze_mem::tag::heap_give_back_settled();
        if matches!(settle, Settle::Settled { .. }) || deadline.passed(clock.now()) {
            return settle;
        }
        let next = Deadline::after(clock.now(), SETTLE_LOOK).min(deadline);
        let what = "every pool worker's heap give-back, before a purge reads the footprint";
        if let Err(error) = blaze_core::wait::sleep_until(what, next, &clock) {
            tracing::warn!(target: "blaze", "a memory purge stopped waiting for {what}: {error}");
            return settle;
        }
    }
}

/// The thread that purges for the daemon's loop ([`purger`]): the loop asks and goes on, and
/// hears the purge done at a later turn, since a purge waits up to [`SETTLE_WITHIN`] for the
/// workers and walks every heap (docs/memory-footprint.md, step 2: "the evictor's thread
/// releases kept memory, never the loop").
pub struct Purger {
    /// Purges asked, so far.
    asked: std::sync::atomic::AtomicU64,
    /// Of `asked`, the most that a purge which began after them has served.
    done: std::sync::atomic::AtomicU64,
    parker: blaze_core::wait::Parker,
    /// Why the purger's thread stopped, once it has: its park failed or a purge panicked.
    /// No later ask is ever served, so [`Purger::done`] says this instead of a count that
    /// would hold every asker waiting.
    stopped: std::sync::OnceLock<String>,
}

impl Purger {
    /// Asks a purge: the ticket that [`Purger::done`] reaches once one that began after this
    /// ask ended. Never waits.
    pub fn ask(&self) -> u64 {
        let ticket = self.asked.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        self.parker.unpark();
        ticket
    }

    /// The tickets served so far (a purge that began after each of them ended), or why the
    /// purger's thread stopped serving them.
    pub fn done(&self) -> Result<u64, &str> {
        match self.stopped.get() {
            Some(why) => Err(why),
            None => Ok(self.done.load(std::sync::atomic::Ordering::Acquire)),
        }
    }

    fn run(&self) {
        use std::sync::atomic::Ordering;
        let clock = blaze_core::wait::Monotonic::new();
        loop {
            let asked = self.asked.load(Ordering::Acquire);
            if asked > self.done.load(Ordering::Relaxed) {
                // A panic fails this purge's askers, never the daemon (rule
                // panics-fail-one-job): they hear why from `done`.
                if let Err(payload) = std::panic::catch_unwind(purge) {
                    let said = stdx::panic_text(&*payload).unwrap_or("no message");
                    let why = format!("the memory purge panicked: {said}"); // alloc: once
                    let _ = self.stopped.set(why);
                    return;
                }
                self.done.store(asked, Ordering::Release);
                continue;
            }
            // The purger's idle: it waits for an ask alone, so no deadline.
            let what = "a memory purge the loop or the high-water mark asks";
            if let Err(error) = self.parker.park(what, None, &clock) {
                let why = format!("the memory purger could not park: {error}"); // alloc: once
                let _ = self.stopped.set(why);
                return;
            }
        }
    }
}

/// The daemon's one [`Purger`], its thread started on first use.
///
/// # Panics
///
/// Where its thread cannot start, naming why: the daemon then has no purge for admission.
pub fn purger() -> &'static Purger {
    static PURGER: std::sync::OnceLock<&'static Purger> = std::sync::OnceLock::new();
    PURGER.get_or_init(|| {
        // alloc: the purger, once a process, for as long as it lives
        let purger: &'static Purger = Box::leak(Box::new(Purger {
            asked: std::sync::atomic::AtomicU64::new(0),
            done: std::sync::atomic::AtomicU64::new(0),
            parker: blaze_core::wait::Parker::new(),
            stopped: std::sync::OnceLock::new(),
        }));
        std::thread::Builder::new()
            .name("blaze-purge".to_owned()) // alloc: a thread's name, once
            .spawn(move || purger.run())
            .unwrap_or_else(|why| panic!("the memory purger's thread could not start: {why}"));
        purger
    })
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

    /// The invariant: the cgroup's footprint is its charge less the file cache the kernel can
    /// drop, and nothing else is taken off. The directed case: a cgroup of 10 GiB of which 6 are
    /// file cache, 1 of that shared memory, is 5 GiB (the control: counting all of `file` as
    /// droppable says 4, counting none says 10). A line that only begins with a field's name
    /// (`file_mapped`, `shmem_thp`) is not the field.
    #[test]
    fn a_cgroups_footprint_is_its_charge_less_the_file_cache_the_kernel_can_drop() {
        assert_eq!(directed_case(super::charged), Some(5 * GIB));
        let stat = directed_stat();
        assert_eq!(super::charged(GIB, stat.as_bytes()), Some(0), "never below nothing");
        assert_eq!(super::charged(10 * GIB, b"anon 5\nfile 3\n"), None, "no shmem line");
        assert_eq!(super::charged(10 * GIB, b"anon 5\nshmem 3\n"), None, "no file line");
    }

    const GIB: u64 = 1 << 30;

    /// The directed case's `memory.stat`: 6 GiB of file cache, 1 of it shared memory, among
    /// lines that only begin with those fields' names.
    fn directed_stat() -> String {
        format!(
            "anon {}\nfile_mapped 7\nfile {}\nkernel 123\nshmem_thp 9\nshmem {}\nfile_dirty 0\n",
            4 * GIB,
            6 * GIB,
            GIB
        )
    }

    /// What `charged` says of the directed case: a cgroup of 10 GiB with [`directed_stat`].
    fn directed_case(charged: fn(u64, &[u8]) -> Option<u64>) -> Option<u64> {
        charged(10 * GIB, directed_stat().as_bytes())
    }

    /// The control: the directed case tells the real rule from the two wrong ones beside it.
    /// Shared memory counted as droppable (all of `file` taken off) says 4 GiB; no file cache
    /// taken off says 10; a field matched by its prefix reads `file_mapped`'s 7 bytes as
    /// `file`.
    #[test]
    fn the_oracle_catches_shared_memory_dropped_and_file_cache_kept() {
        fn field(stat: &[u8], name: &[u8]) -> Option<u64> {
            stat.split(|&it| it == b'\n')
                .find_map(|line| super::decimal(line.strip_prefix(name)?.strip_prefix(b" ")?))
        }
        fn shmem_dropped(current: u64, stat: &[u8]) -> Option<u64> {
            Some(current.saturating_sub(field(stat, b"file")?))
        }
        fn cache_kept(current: u64, _: &[u8]) -> Option<u64> {
            Some(current)
        }
        fn by_prefix(current: u64, stat: &[u8]) -> Option<u64> {
            let first = |name: &[u8]| {
                stat.split(|&it| it == b'\n').find_map(|line| {
                    let rest = line.strip_prefix(name)?;
                    super::decimal(&rest[rest.iter().position(|&it| it == b' ')?..])
                })
            };
            Some(current.saturating_sub(first(b"file")?.saturating_sub(first(b"shmem")?)))
        }
        assert_eq!(directed_case(shmem_dropped), Some(4 * GIB));
        assert_eq!(directed_case(cache_kept), Some(10 * GIB));
        assert_ne!(directed_case(by_prefix), Some(5 * GIB));
    }

    /// A cgroup whose files cannot be read has no footprint: never another measure in its
    /// place.
    #[test]
    fn a_cgroup_that_cannot_be_read_has_no_footprint() {
        let none = std::path::Path::new("/nonexistent-blaze-cgroup/memory.current");
        assert_eq!(super::cgroup_charge(none, none), None);
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
